//! tcpool-sync: keeps Jellyfin's codec offers at the lowest common denominator of every
//! configured pool worker, and keeps Jellyfin's transcode-dir marker present.
//!
//! - All configured workers count, down ones included (last-known outputs, persisted), because a
//!   session started while only the Arc is up must still be able to fail over to the P4 later.
//!   A worker never seen counts as H.264-only until it reports: offering less never breaks a
//!   stream.
//! - Only outputs are constrained; decode, scaling and tonemapping fall back per job.
//!
//! Env: TC_WORKERS, JF_URL (default http://127.0.0.1:8096), JF_API_KEY, TC_CAPS_FILE
//! (default /config/tc-mesh-caps.json), TC_SYNC_EVERY (s, default 30), TC_TRANSCODE_DIR, TC_SYNC_ONCE.

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

fn jf_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .into()
}

/// Reconciles Jellyfin's encoding options with `common`, and returns the (option, applied bool)
/// pairs for `tcpool_jellyfin_offer` -- read from `enc` (Jellyfin's actual config), not just
/// `common`, so the metric reflects Jellyfin's now-current state rather than the value tcpool
/// merely intended to write (which could differ if the POST below had failed).
fn apply_offers(
    common: &[String],
    configured: &[String],
    known: &BTreeMap<String, Known>,
) -> Result<Vec<(&'static str, bool)>, String> {
    let base = env("JF_URL", "http://127.0.0.1:8096");
    let auth = format!("MediaBrowser Token=\"{}\"", env("JF_API_KEY", ""));
    let agent = jf_agent();
    let url = format!("{base}/System/Configuration/encoding");
    let mut enc: serde_json::Value = agent
        .get(&url)
        .header("Authorization", &auth)
        .call()
        .map_err(|e| e.to_string())?
        .body_mut()
        .read_json()
        .map_err(|e| e.to_string())?;
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
            log(&format!("offer {option}: -> {want} ({why})"));
            enc[*option] = serde_json::Value::Bool(want);
            changed = true;
        }
    }
    if common.iter().any(|c| c == "hevc") && !common.iter().any(|c| c == "hevc10") {
        log("note: every worker encodes HEVC 8-bit but not all encode HEVC 10-bit; Jellyfin has no separate 10-bit offer");
    }
    if changed {
        agent
            .post(&url)
            .header("Authorization", &auth)
            .send_json(&enc)
            .map_err(|e| e.to_string())?;
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

async fn sync_once(state: &mut StateFile) -> Result<Vec<(&'static str, bool)>, String> {
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
    let workers = state.workers.clone();
    tokio::task::spawn_blocking(move || apply_offers(&common, &names, &workers))
        .await
        .map_err(|e| e.to_string())?
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
        match sync_once(&mut state).await {
            Ok(offers) => {
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
}
