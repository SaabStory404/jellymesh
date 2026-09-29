//! tcpool-sync: keeps Jellyfin's codec offers at the lowest common denominator of every
//! configured pool worker, and keeps Jellyfin's transcode-dir marker present.
//!
//! - All configured workers count, down ones included (last-known outputs, persisted), because a
//!   session started while only the Arc is up must still be able to fail over to the P4 later.
//!   A worker never seen counts as H.264-only until it reports: offering less never breaks a
//!   stream.
//! - Only outputs are constrained; decode, scaling and tonemapping fall back per job.
//!
//! - Offers are not written until every configured worker has been seen at least once (this
//!   round or persisted in the caps file), or TC_STARTUP_GRACE has passed: a fresh start with no
//!   caps file would otherwise count a slow-to-answer worker as H.264-only and switch HEVC off in
//!   Jellyfin for a cycle. Until then Jellyfin's current values are left alone.
//! - Every Jellyfin in JF_URL is reconciled (one per replica: each keeps its own encoding.xml).
//!
//! Env: TC_WORKERS, JF_URL (comma-separated list, default http://127.0.0.1:8096), JF_API_KEY
//! (shared by all JF_URL targets), TC_CAPS_FILE (default /config/tc-mesh-caps.json),
//! TC_SYNC_EVERY (s, default 30), TC_STARTUP_GRACE (s, default 120), TC_TRANSCODE_DIR,
//! TC_SYNC_ONCE.

mod metrics;

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;
use tcpool_proto::worker_client::WorkerClient;
use tcpool_proto::HelloRequest;
use tonic::transport::Endpoint;

const UNKNOWN: &[&str] = &["h264"];
/// Jellyfin encoding option -> output token that must be common to enable it.
const OFFERS: &[(&str, &str)] = &[("AllowHevcEncoding", "hevc"), ("AllowAv1Encoding", "av1")];

fn log(msg: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let s = now.as_secs() % 86400;
    println!("{:02}:{:02}:{:02} {msg}", s / 3600, (s / 60) % 60, s % 60);
}

fn env(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
struct Known {
    outputs: Vec<String>,
    kind: String,
    seen_unix: i64,
    // Added for metrics: #[serde(default)] so a caps file from before this field existed still
    // deserializes (a missing field would otherwise fail the whole parse and silently wipe
    // last-known outputs -- see the KB gotcha about offers regressing to h264-only).
    #[serde(default)]
    capacity: f64,
    #[serde(default)]
    units_used: f64,
}

#[derive(Serialize, Deserialize, Default, Debug)]
struct StateFile {
    workers: BTreeMap<String, Known>,
    common: Vec<String>,
    live: Vec<String>,
    synced_unix: i64,
    /// Unlike `synced_unix` (set before `apply_offers` runs, i.e. "last attempt"), this is set
    /// only when a full cycle -- Hello round + Jellyfin offer reconciliation -- succeeds.
    #[serde(default)]
    last_success_unix: i64,
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn configured() -> Vec<(String, String)> {
    env("TC_WORKERS", "")
        .split(',')
        .filter_map(|i| {
            i.trim()
                .split_once('=')
                .map(|(n, a)| (n.trim().to_string(), a.trim().to_string()))
        })
        .collect()
}

async fn hello(addr: &str) -> Result<tcpool_proto::Caps, String> {
    let tls = tcpool_proto::tls::TlsFiles::from_env()?
        .map(|f| f.client_config())
        .transpose()?;
    let mut ep = Endpoint::from_shared(tcpool_proto::tls::endpoint_url(addr, tls.is_some()))
        .map_err(|e| e.to_string())?;
    if let Some(t) = tls {
        ep = ep.tls_config(t).map_err(|e| e.to_string())?;
    }
    let ep = ep.connect_timeout(Duration::from_secs(2));
    tokio::time::timeout(Duration::from_secs(3), async {
        let ch = ep.connect().await.map_err(|e| e.to_string())?;
        WorkerClient::new(ch)
            .hello(HelloRequest {})
            .await
            .map(|r| r.into_inner())
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|_| "timeout".to_string())?
}

/// Intersection of outputs over every configured worker (unknown workers = H.264 only).
fn common(configured: &[String], known: &BTreeMap<String, Known>) -> Vec<String> {
    let mut sets = configured.iter().map(|n| {
        known
            .get(n)
            .map(|k| k.outputs.iter().cloned().collect::<BTreeSet<_>>())
            .unwrap_or_else(|| UNKNOWN.iter().map(|s| s.to_string()).collect())
    });
    let Some(first) = sets.next() else {
        return vec![];
    };
    sets.fold(first, |acc, s| acc.intersection(&s).cloned().collect())
        .into_iter()
        .collect()
}

/// Configured workers that have never reported (not in `known`, persisted or fresh).
fn unseen(configured: &[String], known: &BTreeMap<String, Known>) -> Vec<String> {
    configured
        .iter()
        .filter(|n| !known.contains_key(*n))
        .cloned()
        .collect()
}

/// Hold off writing offers while the worker view is incomplete, until `grace` has passed since
/// startup; after that an unseen worker is treated as H.264-only (the conservative default).
fn hold_offers(unseen: &[String], since_start: Duration, grace: Duration) -> bool {
    !unseen.is_empty() && since_start < grace
}

/// JF_URL: one or more Jellyfin base URLs, comma-separated (blank entries ignored).
fn jf_targets(raw: &str) -> Vec<String> {
    let v: Vec<String> = raw
        .split(',')
        .map(|u| u.trim().trim_end_matches('/').to_string())
        .filter(|u| !u.is_empty())
        .collect();
    if v.is_empty() {
        vec!["http://127.0.0.1:8096".to_string()]
    } else {
        v
    }
}

fn jf_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .into()
}

/// Sets `enc`'s offer options from `common`; returns whether anything changed.
fn reconcile(
    enc: &mut serde_json::Value,
    target: &str,
    common: &[String],
    configured: &[String],
    known: &BTreeMap<String, Known>,
) -> bool {
    let mut changed = false;
    for (option, token) in OFFERS {
        let want = common.iter().any(|c| c == token);
        if enc.get(*option).and_then(|v| v.as_bool()) != Some(want) {
            let lacking: Vec<&str> = configured
                .iter()
                .filter(|n| {
                    !known
                        .get(*n)
                        .is_some_and(|k| k.outputs.iter().any(|o| o == token))
                })
                .map(String::as_str)
                .collect();
            let why = if want {
                format!("every worker outputs {token}")
            } else {
                format!("{} cannot output {token}", lacking.join(", "))
            };
            log(&format!("{target}: offer {option}: -> {want} ({why})"));
            enc[*option] = serde_json::Value::Bool(want);
            changed = true;
        }
    }
    changed
}

/// Reconciles one Jellyfin's encoding options with `common` and returns the (option, applied)
/// pairs read back from `enc` (Jellyfin's actual config), not just `common`, so the metric
/// reflects Jellyfin's now-current state rather than the value tcpool merely intended to write.
fn apply_offers_to(
    agent: &ureq::Agent,
    base: &str,
    auth: &str,
    common: &[String],
    configured: &[String],
    known: &BTreeMap<String, Known>,
) -> Result<Vec<(&'static str, bool)>, String> {
    let url = format!("{base}/System/Configuration/encoding");
    let mut enc: serde_json::Value = agent
        .get(&url)
        .header("Authorization", auth)
        .call()
        .map_err(|e| format!("{base}: {e}"))?
        .body_mut()
        .read_json()
        .map_err(|e| format!("{base}: {e}"))?;
    if reconcile(&mut enc, base, common, configured, known) {
        agent
            .post(&url)
            .header("Authorization", auth)
            .send_json(&enc)
            .map_err(|e| format!("{base}: {e}"))?;
    }
    Ok(OFFERS
        .iter()
        .map(|(option, _)| {
            (
                *option,
                enc.get(*option).and_then(|v| v.as_bool()).unwrap_or(false),
            )
        })
        .collect())
}

/// Reconciles every Jellyfin in `targets` (each replica keeps its own encoding.xml). One
/// unreachable target does not stop the others; the cycle fails (and the offer metric keeps its
/// last value) if any target failed. A returned offer is on only if it is on in every target.
fn apply_offers(
    targets: &[String],
    auth: &str,
    common: &[String],
    configured: &[String],
    known: &BTreeMap<String, Known>,
) -> Result<Vec<(&'static str, bool)>, String> {
    let agent = jf_agent();
    if common.iter().any(|c| c == "hevc") && !common.iter().any(|c| c == "hevc10") {
        log("note: every worker encodes HEVC 8-bit but not all encode HEVC 10-bit; Jellyfin has no separate 10-bit offer");
    }
    let mut merged: Vec<(&'static str, bool)> = OFFERS.iter().map(|(o, _)| (*o, true)).collect();
    let mut errors = Vec::new();
    for base in targets {
        match apply_offers_to(&agent, base, auth, common, configured, known) {
            Ok(offers) => {
                for ((_, m), (_, v)) in merged.iter_mut().zip(offers) {
                    *m &= v;
                }
            }
            Err(e) => errors.push(e),
        }
    }
    if errors.is_empty() {
        Ok(merged)
    } else {
        Err(errors.join("; "))
    }
}

/// Jellyfin wipes its transcode dir (marker included) at startup, and every transcode re-creates
/// a missing marker with an exclusive open; two transcodes starting together then race and one
/// gets HTTP 500 (MEASURED on NFS). Keep it present.
fn keep_marker(dir: String) {
    std::thread::spawn(move || {
        let marker = std::path::Path::new(&dir).join(".jellyfin-transcode");
        loop {
            if std::path::Path::new(&dir).is_dir() && !marker.exists() {
                match std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&marker)
                {
                    Ok(_) => log(&format!(
                        "re-created {} (Jellyfin wipes it at startup)",
                        marker.display()
                    )),
                    Err(e) => log(&format!("marker: {e}")),
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

/// One Hello round plus offer reconciliation. `Ok(None)`: offers deliberately not written this
/// cycle (incomplete worker view during the startup grace).
async fn sync_once(
    state: &mut StateFile,
    since_start: Duration,
    grace: Duration,
) -> Result<Option<Vec<(&'static str, bool)>>, String> {
    let cfg = configured();
    let names: Vec<String> = cfg.iter().map(|(n, _)| n.clone()).collect();
    let mut live = Vec::new();
    for (name, addr) in &cfg {
        match hello(addr).await {
            Ok(caps) => {
                let prev = state.workers.get(name).map(|k| k.outputs.clone());
                if prev.as_ref() != Some(&caps.outputs) {
                    log(&format!(
                        "worker {name}: outputs {:?} (was {prev:?})",
                        caps.outputs
                    ));
                }
                state.workers.insert(
                    name.clone(),
                    Known {
                        outputs: caps.outputs,
                        kind: caps.kind,
                        seen_unix: now_unix(),
                        capacity: caps.capacity,
                        units_used: caps.units_used,
                    },
                );
                live.push(name.clone());
            }
            Err(e) if !state.workers.contains_key(name) => {
                log(&format!(
                    "worker {name} never seen ({e}); counting it as {UNKNOWN:?} until it reports"
                ));
            }
            Err(_) => {}
        }
    }
    let common = common(&names, &state.workers);
    if !common.iter().any(|c| c == "h264") {
        log(&format!(
            "WARNING: not every worker can output h264 ({:?})",
            state.workers
        ));
    }
    state.common = common.clone();
    state.live = live;
    state.synced_unix = now_unix();
    let missing = unseen(&names, &state.workers);
    if hold_offers(&missing, since_start, grace) {
        log(&format!(
            "not writing offers yet: no report from {} ({}s of {}s startup grace)",
            missing.join(", "),
            since_start.as_secs(),
            grace.as_secs()
        ));
        return Ok(None);
    }
    let workers = state.workers.clone();
    let targets = jf_targets(&env("JF_URL", ""));
    let auth = format!("MediaBrowser Token=\"{}\"", env("JF_API_KEY", ""));
    tokio::task::spawn_blocking(move || apply_offers(&targets, &auth, &common, &names, &workers))
        .await
        .map_err(|e| e.to_string())?
        .map(Some)
}

/// Rebuild the metrics snapshot from the latest sync state. Called after every cycle (success or
/// failure) so `tcpool_pool_*` reflects the latest Hello round even when `apply_offers` itself
/// failed; `offers` (which needs a successful Jellyfin round-trip) is only replaced on success,
/// otherwise the previous known-good values are kept.
fn refresh_metrics(
    shared: &metrics::Shared,
    state: &StateFile,
    configured: &[(String, String)],
    offers: Option<&[(&'static str, bool)]>,
) {
    let mut snap = shared.lock().unwrap_or_else(|p| p.into_inner());
    snap.workers_configured = configured.len();
    snap.workers = configured
        .iter()
        .map(|(name, _)| {
            let info = state
                .workers
                .get(name)
                .map(|k| metrics::WorkerInfo {
                    capacity: k.capacity,
                    units_used: k.units_used,
                    live: state.live.contains(name),
                })
                .unwrap_or_default();
            (name.clone(), info)
        })
        .collect();
    snap.common = state.common.clone();
    if let Some(offers) = offers {
        snap.offers = offers.to_vec();
        snap.last_success_unix = state.last_success_unix;
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Ok(dir) = std::env::var("TC_TRANSCODE_DIR") {
        if !dir.is_empty() {
            keep_marker(dir);
        }
    }
    let caps_file = env("TC_CAPS_FILE", "/config/tc-mesh-caps.json");
    let every = Duration::from_secs(env("TC_SYNC_EVERY", "30").parse().unwrap_or(30));
    let grace = Duration::from_secs(env("TC_STARTUP_GRACE", "120").parse().unwrap_or(120));
    let started = std::time::Instant::now();
    let mut state: StateFile = std::fs::read_to_string(&caps_file)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let shared_metrics: metrics::Shared = Default::default();
    if let Some(port) = std::env::var("TC_METRICS_PORT")
        .ok()
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<u16>().ok())
    {
        let shared = shared_metrics.clone();
        tokio::spawn(async move { metrics::serve(shared, port).await });
    }

    loop {
        let cfg = configured();
        match sync_once(&mut state, started.elapsed(), grace).await {
            Ok(None) => refresh_metrics(&shared_metrics, &state, &cfg, None),
            Ok(Some(offers)) => {
                state.last_success_unix = now_unix();
                refresh_metrics(&shared_metrics, &state, &cfg, Some(&offers));
            }
            Err(e) => {
                log(&format!("sync failed: {e}"));
                refresh_metrics(&shared_metrics, &state, &cfg, None);
            }
        }
        if let Ok(s) = serde_json::to_string_pretty(&state) {
            let tmp = format!("{caps_file}.tmp");
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(&tmp, &caps_file);
            }
        }
        if std::env::var_os("TC_SYNC_ONCE").is_some() {
            println!(
                "{}",
                serde_json::to_string_pretty(&state).unwrap_or_default()
            );
            return;
        }
        tokio::time::sleep(every).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn unknown_workers_are_h264_only() {
        let mut known = BTreeMap::new();
        known.insert(
            "qsv".into(),
            Known {
                outputs: vec!["h264".into(), "hevc".into(), "av1".into()],
                ..Default::default()
            },
        );
        known.insert(
            "nv".into(),
            Known {
                outputs: vec!["h264".into(), "hevc".into()],
                ..Default::default()
            },
        );
        assert_eq!(
            common(&["qsv".into(), "nv".into()], &known),
            vec!["h264", "hevc"]
        );
        assert_eq!(
            common(&["qsv".into(), "nv".into(), "new".into()], &known),
            vec!["h264"]
        );
    }

    fn known_with(outputs: &[&str]) -> Known {
        Known {
            outputs: outputs.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn offers_held_until_every_worker_seen_or_grace_over() {
        let mut known = BTreeMap::new();
        known.insert("qsv".to_string(), known_with(&["h264", "hevc"]));
        let cfg = vec!["qsv".to_string(), "nvenc".to_string()];
        let missing = unseen(&cfg, &known);
        assert_eq!(missing, vec!["nvenc"]);
        let grace = Duration::from_secs(120);
        assert!(hold_offers(&missing, Duration::from_secs(0), grace));
        assert!(hold_offers(&missing, Duration::from_secs(119), grace));
        // Grace over: fall back to "unseen = H.264 only".
        assert!(!hold_offers(&missing, Duration::from_secs(120), grace));
        // Complete view (persisted entries count): write at once.
        known.insert("nvenc".to_string(), known_with(&["h264", "hevc"]));
        assert!(unseen(&cfg, &known).is_empty());
        assert!(!hold_offers(&unseen(&cfg, &known), Duration::ZERO, grace));
    }

    #[test]
    fn jf_url_is_a_list() {
        assert_eq!(
            jf_targets("http://a:8096, http://b:8096/ ,,"),
            vec!["http://a:8096", "http://b:8096"]
        );
        assert_eq!(jf_targets("http://a:8096"), vec!["http://a:8096"]);
        assert_eq!(jf_targets(""), vec!["http://127.0.0.1:8096"]);
    }

    /// Minimal Jellyfin stand-in: GET returns `enc`, POST bodies are recorded. Serves until the
    /// test ends; every response closes the connection.
    fn fake_jellyfin(enc: serde_json::Value) -> (String, Arc<Mutex<Vec<String>>>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let log: Arc<Mutex<Vec<String>>> = Default::default();
        let seen = log.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut r = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                let mut len = 0usize;
                loop {
                    let mut h = String::new();
                    r.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0u8; len];
                r.read_exact(&mut body).unwrap();
                let method = line.split(' ').next().unwrap_or("").to_string();
                seen.lock()
                    .unwrap()
                    .push(format!("{method} {}", String::from_utf8_lossy(&body)));
                let resp = if method == "GET" {
                    let b = enc.to_string();
                    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}", b.len())
                } else {
                    "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".to_string()
                };
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        (base, log)
    }

    #[test]
    fn every_jellyfin_target_is_written() {
        let enc =
            serde_json::json!({"AllowHevcEncoding": false, "AllowAv1Encoding": false, "Keep": 7});
        let (qsv, qsv_log) = fake_jellyfin(enc.clone());
        let (nvenc, nvenc_log) = fake_jellyfin(enc);
        let mut known = BTreeMap::new();
        known.insert("qsv".to_string(), known_with(&["h264", "hevc", "av1"]));
        known.insert("nvenc".to_string(), known_with(&["h264", "hevc"]));
        let cfg = vec!["qsv".to_string(), "nvenc".to_string()];
        let common = common(&cfg, &known);
        let offers = apply_offers(&[qsv, nvenc], "t", &common, &cfg, &known).unwrap();
        assert_eq!(
            offers,
            vec![("AllowHevcEncoding", true), ("AllowAv1Encoding", false)]
        );
        for log in [qsv_log, nvenc_log] {
            let log = log.lock().unwrap();
            assert_eq!(log.len(), 2, "{log:?}");
            assert!(log[0].starts_with("GET"));
            let posted: serde_json::Value =
                serde_json::from_str(log[1].strip_prefix("POST ").unwrap()).unwrap();
            assert_eq!(posted["AllowHevcEncoding"], true);
            assert_eq!(posted["AllowAv1Encoding"], false);
            assert_eq!(posted["Keep"], 7, "other encoding options preserved");
        }
    }

    #[test]
    fn one_unreachable_target_does_not_block_the_other() {
        let (up, up_log) = fake_jellyfin(serde_json::json!({"AllowHevcEncoding": false}));
        let down = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", l.local_addr().unwrap())
        }; // listener dropped: connection refused
        let mut known = BTreeMap::new();
        known.insert("qsv".to_string(), known_with(&["h264", "hevc"]));
        let cfg = vec!["qsv".to_string()];
        let err = apply_offers(
            &[down.clone(), up],
            "t",
            &["h264".into(), "hevc".into()],
            &cfg,
            &known,
        )
        .unwrap_err();
        assert!(err.contains(&down), "{err}");
        assert_eq!(
            up_log.lock().unwrap().len(),
            2,
            "reachable target still written"
        );
    }

    /// The startup bug: no caps file, a worker that doesn't answer the first Hello. Before the fix
    /// this wrote AllowHevcEncoding=false to Jellyfin; now Jellyfin is not contacted at all.
    #[tokio::test]
    async fn fresh_start_with_silent_worker_writes_nothing() {
        let (jf, jf_log) = fake_jellyfin(serde_json::json!({"AllowHevcEncoding": true}));
        let dead = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().to_string()
        };
        // Only this test sets these variables.
        std::env::set_var("TC_WORKERS", format!("nvenc={dead}"));
        std::env::set_var("JF_URL", &jf);
        let mut state = StateFile::default();
        let r = sync_once(&mut state, Duration::ZERO, Duration::from_secs(120))
            .await
            .unwrap();
        assert!(r.is_none());
        assert!(
            jf_log.lock().unwrap().is_empty(),
            "Jellyfin must not be touched"
        );
        // Grace over: the conservative write happens.
        let r = sync_once(
            &mut state,
            Duration::from_secs(120),
            Duration::from_secs(120),
        )
        .await
        .unwrap();
        assert_eq!(r.unwrap()[0], ("AllowHevcEncoding", false));
        std::env::remove_var("TC_WORKERS");
        std::env::remove_var("JF_URL");
    }
}
