//! tcpool-sync: keeps Jellyfin's codec offers at the lowest common denominator of every
//! configured pool worker, and keeps Jellyfin's transcode-dir marker present.
//!
//! - All configured workers count, down ones included (last-known outputs, persisted), because a
//!   session started while only the Arc is up must still be able to fail over to the P4 later.
//!   A worker never seen counts as H.264-only until it reports: offering less never breaks a
//!   stream.
//! - Only outputs are constrained; decode, scaling and tonemapping fall back per job.
//! - Worker discovery mirrors the shim (crates/shim/src/main.rs `targets`): `TC_WORKERS_DNS` (a
//!   headless Service; every A/AAAA record is one agent) is resolved first, then any `TC_WORKERS`
//!   entry for an address DNS did not return is merged in, so the static list still works during a
//!   resolver outage.
//! - Offers are not written until every worker this cycle knows about (`worker_set`: this round's
//!   targets plus every worker already persisted) has been seen at least once, or TC_STARTUP_GRACE
//!   has passed: a fresh start with no caps file would otherwise count a slow-to-answer worker as
//!   H.264-only and switch HEVC off in Jellyfin for a cycle. Until then Jellyfin's current values
//!   are left alone.
//! - Every Jellyfin in JF_URL is reconciled (one per replica: each keeps its own encoding.xml). An
//!   offer is `effective = the user's choice AND the pool can`, ANDed again across every JF_URL
//!   target so the metric/`/status` reflect the strictest replica; one unreachable target does not
//!   block the others.
//! - When it is off, why it is off (the user disabled it vs a configured worker cannot output it)
//!   is recorded and served on `/status` (crate::status).
//!
//! Env: TC_WORKERS_DNS, TC_WORKERS, JF_URL (comma-separated list, default http://127.0.0.1:8096),
//! JF_API_KEY (shared by all JF_URL targets), TC_CAPS_FILE (default /config/tc-mesh-caps.json),
//! TC_SYNC_EVERY (s, default 30), TC_STARTUP_GRACE (s, default 120), TC_TRANSCODE_DIR,
//! TC_SYNC_ONCE, TC_METRICS_PORT (Prometheus metrics and the /status JSON on one port).

mod metrics;
mod status;

use serde::{Deserialize, Serialize};
use status::Offer;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;
use tcpool_proto::worker_client::WorkerClient;
use tcpool_proto::HelloRequest;
use tonic::transport::Endpoint;

const UNKNOWN: &[&str] = &["h264"];
/// Jellyfin encoding option -> output token that must be common to enable it.
const OFFERS: &[(&str, &str)] = &[("AllowHevcEncoding", "hevc"), ("AllowAv1Encoding", "av1")];

/// How many times a `TC_WORKERS_DNS` lookup is retried before the round gives up (shim parity). A
/// headless Service with no Ready endpoint is NXDOMAIN, which getaddrinfo reports exactly like a
/// CoreDNS blip, so the two cannot be told apart here: retry, then fall through to the static list.
const DNS_ATTEMPTS: usize = 3;
const DNS_RETRY: Duration = Duration::from_millis(300);

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

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One dialable agent. `name` is the identity used to key the persisted state: the address for a
/// DNS record (that is all DNS gives us), the operator's name for a `TC_WORKERS` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    name: String,
    addr: String,
    /// `dns` or `static`; surfaced in `/status` so a stale entry is identifiable.
    source: &'static str,
}

fn configured_workers() -> Vec<Target> {
    env("TC_WORKERS", "")
        .split(',')
        .filter_map(|item| {
            let (name, addr) = item.trim().split_once('=')?;
            Some(Target {
                name: name.trim().to_string(),
                addr: addr.trim().to_string(),
                source: "static",
            })
        })
        .collect()
}

/// Parse `TC_WORKERS_DNS` (`host`, `host:port`, `[v6]:port`) into the pair `lookup_host` wants.
/// The fully qualified cluster form keeps its trailing dot, which is what stops the resolver
/// walking the search list. Missing port = the agent default.
fn parse_dns_target(spec: &str) -> Option<(String, u16)> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    if let Some(rest) = spec.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        let port = match tail {
            "" => tcpool_proto::DEFAULT_PORT,
            t => t.strip_prefix(':')?.parse().ok()?,
        };
        return Some((host.to_string(), port));
    }
    match spec.rsplit_once(':') {
        // An unbracketed IPv6 literal has more than one colon: ambiguous, so refuse it.
        Some((host, port)) if !host.is_empty() && !host.contains(':') => {
            Some((host.to_string(), port.parse().ok()?))
        }
        Some(_) => None,
        None => Some((spec.to_string(), tcpool_proto::DEFAULT_PORT)),
    }
}

/// Resolve every address behind the headless Service. Each record is one agent pod, so the pool
/// grows and shrinks with the DaemonSet and a draining agent leaves DNS on its own (its readiness
/// probe fails, so the Service drops it). The name is the address: with TLS the certificate is
/// still checked against `TC_TLS_SERVER_NAME`, which `client_config` pins as the domain name, so
/// dialling an IP is fine.
async fn resolve_dns(host: &str, port: u16) -> Result<Vec<Target>, String> {
    let mut last = String::new();
    for attempt in 0..DNS_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(DNS_RETRY).await;
        }
        match tokio::net::lookup_host((host, port)).await {
            Ok(found) => {
                let mut addrs: Vec<std::net::SocketAddr> = found.collect();
                addrs.sort();
                addrs.dedup();
                return Ok(addrs
                    .into_iter()
                    .map(|a| Target {
                        name: a.to_string(),
                        addr: a.to_string(),
                        source: "dns",
                    })
                    .collect());
            }
            Err(e) => last = e.to_string(),
        }
    }
    Err(last)
}

/// DNS records first, then any `TC_WORKERS` entry for an address DNS did not already give us, so a
/// static fallback list still works during a resolver outage.
fn merge_targets(dns: Vec<Target>, statics: Vec<Target>) -> Vec<Target> {
    let mut out = dns;
    for t in statics {
        if !out.iter().any(|e| e.addr == t.addr) {
            out.push(t);
        }
    }
    out
}

/// Every agent to try this round.
async fn targets() -> Vec<Target> {
    let dns = match std::env::var("TC_WORKERS_DNS")
        .ok()
        .as_deref()
        .and_then(parse_dns_target)
    {
        None => Vec::new(),
        Some((host, port)) => match resolve_dns(&host, port).await {
            Ok(t) => t,
            Err(e) => {
                log(&format!(
                    "TC_WORKERS_DNS {host}:{port} did not resolve: {e}"
                ));
                Vec::new()
            }
        },
    };
    merge_targets(dns, configured_workers())
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
    /// Last address we reached this worker at, for the status page (a persisted-only worker has no
    /// address in this cycle's target list).
    #[serde(default)]
    addr: String,
    /// The agent's own `TC_NAME` (`Caps.name`), e.g. `qsv-dl380`: what the operator recognises.
    #[serde(default)]
    reported_name: String,
}

/// The persisted record of one offer's user intent. Without this, `effective = intent AND pool-can`
/// cannot survive a `pool-can` that flips off and back on: Jellyfin only stores the effective
/// value, so the user's choice would be lost the moment a worker could not do it.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq, Eq)]
struct OfferState {
    /// Jellyfin's checkbox as the user left it.
    intent: bool,
    /// What tcpool last left in Jellyfin's config. `None` = never recorded (a caps file from
    /// before this field existed), in which case the first cycle trusts Jellyfin's own value.
    #[serde(default)]
    applied: Option<bool>,
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
    /// Keyed by Jellyfin's encoding option name (`OFFERS`). `#[serde(default)]` so an upgrade from
    /// a caps file without it reads as "never recorded" rather than failing the parse.
    #[serde(default)]
    offers: BTreeMap<String, OfferState>,
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

/// The workers a cycle's intersection runs over: every target found this round, plus every worker
/// already known from an earlier round. The union is the point: a worker whose pod has gone away
/// (its address is no longer in DNS) must still constrain the offers, or a session started while
/// only the Arc was up could not fail over to the P4 later. It only ever makes the intersection
/// smaller, which never breaks a stream.
fn worker_set(targets: &[Target], known: &BTreeMap<String, Known>) -> Vec<String> {
    let mut names: BTreeSet<String> = targets.iter().map(|t| t.name.clone()).collect();
    names.extend(known.keys().cloned());
    names.into_iter().collect()
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

/// Which configured workers cannot output `token`. A worker that has never reported is assumed
/// h264-only -- the same fallback `common` uses, so the two never disagree about a worker.
fn lacking(configured: &[String], known: &BTreeMap<String, Known>, token: &str) -> Vec<String> {
    configured
        .iter()
        .filter(|n| {
            let outputs = known.get(*n).map(|k| k.outputs.as_slice());
            !outputs
                .map(|o| o.iter().any(|o| o == token))
                .unwrap_or_else(|| UNKNOWN.contains(&token))
        })
        .cloned()
        .collect()
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

/// The pure core of one offer: user intent, the pool's capability, and the decision, with no
/// Jellyfin round-trip in the way so it can be tested directly.
///
/// Intent comes from `prev` (persisted) and `raw` (Jellyfin's current checkbox):
/// - no record yet: trust `raw`. That is the upgrade path -- a running sync has been writing the
///   effective value, and intent == effective whenever the pool allows it, so nothing is lost.
/// - a record: a `raw` that differs from what tcpool last wrote (`applied`) means the user moved
///   the checkbox; otherwise the stored intent stands, which is what makes the user's choice
///   survive a worker that cannot do it (sync forces the checkbox off, and without this the next
///   cycle would read that off back as "the user disabled it").
///
/// Known residual: a user turning the checkbox off *while* sync is holding it off is a no-op in
/// Jellyfin's state, so it is invisible here; intent stays on and the offer returns when the pool
/// can. An explicit intent control (PLAN §11, P4 controls) is the fix.
fn decide(
    option: &'static str,
    token: &'static str,
    raw: Option<bool>,
    pool_can: bool,
    lacking: &[String],
    prev: Option<&OfferState>,
) -> (Offer, OfferState) {
    let intent = match (prev, raw) {
        (Some(p), Some(r)) => match p.applied {
            Some(a) if r != a => r,
            _ => p.intent,
        },
        (Some(p), None) => p.intent,
        (None, Some(r)) => r,
        (None, None) => false,
    };
    let effective = intent && pool_can;
    let cause = if effective {
        "enabled"
    } else if !intent {
        "user_disabled"
    } else {
        "pool_cannot"
    };
    let reason = if effective {
        format!("the user allows {token} and every configured worker outputs it")
    } else if !intent {
        if pool_can {
            format!("the user disabled {token} (the pool could output it)")
        } else {
            format!(
                "the user disabled {token}; {} cannot output it either",
                lacking.join(", ")
            )
        }
    } else {
        format!("{} cannot output {token}", lacking.join(", "))
    };
    (
        Offer {
            option,
            token,
            intent,
            effective,
            cause,
            reason,
            lacking: lacking.to_vec(),
        },
        OfferState {
            intent,
            applied: Some(effective),
        },
    )
}

/// Reconciles one Jellyfin's encoding options with `common`/`decide`'s per-offer intent, and
/// returns the offers read back from `enc` (Jellyfin's actual config), not just the value tcpool
/// intended to write, so the result reflects Jellyfin's now-current state rather than a POST that
/// silently no-opped -- plus the next `OfferState` to persist for this target's intent tracking.
fn apply_offers_to(
    agent: &ureq::Agent,
    base: &str,
    auth: &str,
    common: &[String],
    configured: &[String],
    known: &BTreeMap<String, Known>,
    prev_state: &BTreeMap<String, OfferState>,
) -> Result<(Vec<Offer>, BTreeMap<String, OfferState>), String> {
    let url = format!("{base}/System/Configuration/encoding");
    let mut enc: serde_json::Value = agent
        .get(&url)
        .header("Authorization", auth)
        .call()
        .map_err(|e| format!("{base}: {e}"))?
        .body_mut()
        .read_json()
        .map_err(|e| format!("{base}: {e}"))?;
    let mut changed = false;
    let mut offers = Vec::new();
    let mut next_state = BTreeMap::new();
    for (option, token) in OFFERS {
        let raw = enc.get(*option).and_then(|v| v.as_bool());
        let pool_can = common.iter().any(|c| c == token);
        let missing = lacking(configured, known, token);
        let (offer, state) = decide(
            option,
            token,
            raw,
            pool_can,
            &missing,
            prev_state.get(*option),
        );
        if raw != Some(offer.effective) {
            log(&format!(
                "{base}: offer {option}: -> {} ({})",
                offer.effective, offer.reason
            ));
            enc[*option] = serde_json::Value::Bool(offer.effective);
            changed = true;
        }
        next_state.insert((*option).to_string(), state);
        offers.push(offer);
    }
    if changed {
        agent
            .post(&url)
            .header("Authorization", auth)
            .send_json(&enc)
            .map_err(|e| format!("{base}: {e}"))?;
    }
    for offer in &mut offers {
        offer.effective = enc
            .get(offer.option)
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    }
    Ok((offers, next_state))
}

/// Reconciles every Jellyfin in `targets` (each replica keeps its own encoding.xml). One
/// unreachable target does not stop the others; the cycle fails (and the offer metric keeps its
/// last value) if any target failed. A returned offer is `effective` only if it is on in every
/// target; the persisted `OfferState` (intent tracking) is taken from the first target that
/// answered, since intent is the operator's single choice, not a per-replica one.
fn apply_offers(
    targets: &[String],
    auth: &str,
    common: &[String],
    configured: &[String],
    known: &BTreeMap<String, Known>,
    prev_state: &BTreeMap<String, OfferState>,
) -> Result<(Vec<Offer>, BTreeMap<String, OfferState>), String> {
    let agent = jf_agent();
    if common.iter().any(|c| c == "hevc") && !common.iter().any(|c| c == "hevc10") {
        log("note: every worker encodes HEVC 8-bit but not all encode HEVC 10-bit; Jellyfin has no separate 10-bit offer");
    }
    let mut merged: Option<Vec<Offer>> = None;
    let mut merged_state: Option<BTreeMap<String, OfferState>> = None;
    let mut errors = Vec::new();
    for base in targets {
        match apply_offers_to(&agent, base, auth, common, configured, known, prev_state) {
            Ok((offers, state)) => {
                match &mut merged {
                    None => merged = Some(offers),
                    Some(m) => {
                        for (mo, o) in m.iter_mut().zip(offers) {
                            mo.effective &= o.effective;
                        }
                    }
                }
                merged_state.get_or_insert(state);
            }
            Err(e) => errors.push(e),
        }
    }
    if errors.is_empty() {
        Ok((merged.unwrap_or_default(), merged_state.unwrap_or_default()))
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
    targets: &[Target],
    since_start: Duration,
    grace: Duration,
) -> Result<Option<Vec<Offer>>, String> {
    let mut live = Vec::new();
    for Target { name, addr, .. } in targets {
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
                        addr: addr.clone(),
                        reported_name: caps.name,
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
    let names = worker_set(targets, &state.workers);
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
    let prev_offers = state.offers.clone();
    let jf = jf_targets(&env("JF_URL", ""));
    let auth = format!("MediaBrowser Token=\"{}\"", env("JF_API_KEY", ""));
    let (offers, next_state) = tokio::task::spawn_blocking(move || {
        apply_offers(&jf, &auth, &common, &names, &workers, &prev_offers)
    })
    .await
    .map_err(|e| e.to_string())??;
    state.offers = next_state;
    Ok(Some(offers))
}

/// Rebuild the metrics/status snapshot from the latest sync state and this cycle's discovery.
/// Called after every cycle (success or failure) so the worker gauges reflect the latest Hello
/// round even when `apply_offers` itself failed; `offers` (which needs a successful Jellyfin
/// round-trip) is only replaced on success, otherwise the previous known-good values are kept.
fn refresh_metrics(
    shared: &metrics::Shared,
    state: &StateFile,
    targets: &[Target],
    offers: Option<&[Offer]>,
) {
    let mut workers: BTreeMap<String, metrics::WorkerInfo> = BTreeMap::new();
    for t in targets {
        let k = state.workers.get(&t.name);
        workers.insert(
            t.name.clone(),
            metrics::WorkerInfo {
                addr: t.addr.clone(),
                source: t.source.to_string(),
                kind: k.map(|k| k.kind.clone()).unwrap_or_default(),
                outputs: k
                    .map(|k| k.outputs.clone())
                    .unwrap_or_else(|| UNKNOWN.iter().map(|s| s.to_string()).collect()),
                capacity: k.map(|k| k.capacity).unwrap_or(0.0),
                units_used: k.map(|k| k.units_used).unwrap_or(0.0),
                live: state.live.iter().any(|l| l == &t.name),
                seen_unix: k.map(|k| k.seen_unix).unwrap_or(0),
            },
        );
    }
    // Workers only in the caps file: gone from DNS, but still counted in the intersection.
    for (name, k) in &state.workers {
        workers
            .entry(name.clone())
            .or_insert_with(|| metrics::WorkerInfo {
                addr: k.addr.clone(),
                source: "persisted".into(),
                kind: k.kind.clone(),
                outputs: k.outputs.clone(),
                capacity: k.capacity,
                units_used: k.units_used,
                live: false,
                seen_unix: k.seen_unix,
            });
    }
    let mut snap = shared.lock().unwrap_or_else(|p| p.into_inner());
    snap.workers_configured = workers.len();
    snap.workers = workers;
    snap.common = state.common.clone();
    snap.synced_unix = state.synced_unix;
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
        let targets = targets().await;
        match sync_once(&mut state, &targets, started.elapsed(), grace).await {
            Ok(None) => refresh_metrics(&shared_metrics, &state, &targets, None),
            Ok(Some(offers)) => {
                state.last_success_unix = now_unix();
                refresh_metrics(&shared_metrics, &state, &targets, Some(&offers));
            }
            Err(e) => {
                log(&format!("sync failed: {e}"));
                refresh_metrics(&shared_metrics, &state, &targets, None);
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

    fn known(outputs: &[&str]) -> Known {
        Known {
            outputs: outputs.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn t(name: &str, addr: &str, source: &'static str) -> Target {
        Target {
            name: name.into(),
            addr: addr.into(),
            source,
        }
    }

    fn lacking_names(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn unknown_workers_are_h264_only() {
        let mut known_map = BTreeMap::new();
        known_map.insert("qsv".into(), known(&["h264", "hevc", "av1"]));
        known_map.insert("nv".into(), known(&["h264", "hevc"]));
        assert_eq!(
            common(&["qsv".into(), "nv".into()], &known_map),
            vec!["h264", "hevc"]
        );
        assert_eq!(
            common(&["qsv".into(), "nv".into(), "new".into()], &known_map),
            vec!["h264"]
        );
    }

    #[test]
    fn dns_spec_parsing_matches_the_shim() {
        assert_eq!(
            parse_dns_target("tcpool-agents.media.svc.cluster.local."),
            Some(("tcpool-agents.media.svc.cluster.local.".into(), 9901))
        );
        assert_eq!(
            parse_dns_target("tcpool-agents.media.svc.cluster.local.:9901"),
            Some(("tcpool-agents.media.svc.cluster.local.".into(), 9901))
        );
        assert_eq!(
            parse_dns_target(" tcpool-agents:19901 "),
            Some(("tcpool-agents".into(), 19901))
        );
        assert_eq!(
            parse_dns_target("[fd00::1]:9901"),
            Some(("fd00::1".into(), 9901))
        );
        assert_eq!(
            parse_dns_target("[fd00::1]"),
            Some(("fd00::1".into(), tcpool_proto::DEFAULT_PORT))
        );
        assert_eq!(parse_dns_target(""), None);
        assert_eq!(parse_dns_target("   "), None);
        // An unbracketed IPv6 literal is ambiguous, and garbage ports are refused.
        assert_eq!(parse_dns_target("fd00::1:9901"), None);
        assert_eq!(parse_dns_target("tcpool-agents:not-a-port"), None);
        assert_eq!(parse_dns_target(":9901"), None);
        assert_eq!(parse_dns_target("[]:9901"), None);
    }

    #[test]
    fn static_workers_are_merged_in_only_when_dns_did_not_return_them() {
        let dns = vec![
            t("10.42.0.7:9901", "10.42.0.7:9901", "dns"),
            t("10.42.0.9:9901", "10.42.0.9:9901", "dns"),
        ];
        let statics = vec![
            // Same address DNS already gave us: DNS wins, so the operator's name does not shadow
            // the record (their outputs are the same worker).
            t("qsv-dl380", "10.42.0.7:9901", "static"),
            // Not in DNS: kept, so the pool still works during a resolver outage.
            t("cpu", "127.0.0.1:9901", "static"),
        ];
        assert_eq!(
            merge_targets(dns, statics),
            vec![
                t("10.42.0.7:9901", "10.42.0.7:9901", "dns"),
                t("10.42.0.9:9901", "10.42.0.9:9901", "dns"),
                t("cpu", "127.0.0.1:9901", "static"),
            ]
        );
        // No DNS at all: the static list is the whole pool (shim parity).
        assert_eq!(
            merge_targets(Vec::new(), vec![t("cpu", "127.0.0.1:9901", "static")]),
            vec![t("cpu", "127.0.0.1:9901", "static")]
        );
    }

    #[test]
    fn worker_set_keeps_a_worker_that_left_dns() {
        // The P4's pod is gone, so its address is no longer a DNS record: it must still count, or
        // the Arc-only intersection would re-enable HEVC for sessions that could fail over to it.
        let mut known_map = BTreeMap::new();
        known_map.insert("10.42.0.9:9901".to_string(), known(&["h264", "hevc"]));
        let targets = vec![t("10.42.0.7:9901", "10.42.0.7:9901", "dns")];
        let set = worker_set(&targets, &known_map);
        assert_eq!(set, vec!["10.42.0.7:9901", "10.42.0.9:9901"]);
        // And the gone worker still holds HEVC out of the intersection.
        assert_eq!(common(&set, &known_map), vec!["h264"]);
    }

    #[test]
    fn offers_held_until_every_worker_seen_or_grace_over() {
        let mut known_map = BTreeMap::new();
        known_map.insert("qsv".to_string(), known(&["h264", "hevc"]));
        let cfg = vec!["qsv".to_string(), "nvenc".to_string()];
        let missing = unseen(&cfg, &known_map);
        assert_eq!(missing, vec!["nvenc"]);
        let grace = Duration::from_secs(120);
        assert!(hold_offers(&missing, Duration::from_secs(0), grace));
        assert!(hold_offers(&missing, Duration::from_secs(119), grace));
        // Grace over: fall back to "unseen = H.264 only".
        assert!(!hold_offers(&missing, Duration::from_secs(120), grace));
        // Complete view (persisted entries count): write at once.
        known_map.insert("nvenc".to_string(), known(&["h264", "hevc"]));
        assert!(unseen(&cfg, &known_map).is_empty());
        assert!(!hold_offers(
            &unseen(&cfg, &known_map),
            Duration::ZERO,
            grace
        ));
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

    #[test]
    fn first_run_trusts_jellyfin_and_reports_the_pool_as_the_blocker() {
        let (offer, state) = decide(
            "AllowHevcEncoding",
            "hevc",
            Some(true),
            false,
            &lacking_names(&["p4"]),
            None,
        );
        assert!(offer.intent, "Jellyfin's own value is the user's choice");
        assert!(!offer.effective);
        assert_eq!(offer.cause, "pool_cannot");
        assert_eq!(offer.lacking, lacking_names(&["p4"]));
        assert!(offer.reason.contains("p4"));
        assert_eq!(state.applied, Some(false));
        assert!(state.intent);
    }

    #[test]
    fn user_disabled_wins_even_when_the_pool_could_output_it() {
        let (offer, state) = decide(
            "AllowAv1Encoding",
            "av1",
            Some(false),
            true,
            &[],
            Some(&OfferState {
                intent: false,
                applied: Some(false),
            }),
        );
        assert!(!offer.effective);
        assert_eq!(offer.cause, "user_disabled");
        assert!(offer.reason.contains("the user disabled av1"));
        assert_eq!(state.applied, Some(false));
    }

    #[test]
    fn intent_survives_a_pool_can_flip() {
        // Sync forced the checkbox off because the P4 cannot do HEVC; Jellyfin still reads false.
        // That must not be mistaken for the user disabling it.
        let prev = OfferState {
            intent: true,
            applied: Some(false),
        };
        let (offer, state) = decide(
            "AllowHevcEncoding",
            "hevc",
            Some(false),
            true,
            &[],
            Some(&prev),
        );
        assert!(offer.intent, "the user's choice is still on");
        assert!(offer.effective, "and the offer comes back with the pool");
        assert_eq!(offer.cause, "enabled");
        assert_eq!(
            state,
            OfferState {
                intent: true,
                applied: Some(true)
            }
        );

        // Still cannot: reported as the pool's fault, with the worker named.
        let (offer, _) = decide(
            "AllowHevcEncoding",
            "hevc",
            Some(false),
            false,
            &lacking_names(&["p4"]),
            Some(&prev),
        );
        assert_eq!(offer.cause, "pool_cannot");
        assert!(offer.reason.contains("p4 cannot output hevc"));
    }

    #[test]
    fn a_value_we_did_not_write_is_the_user_moving_the_checkbox() {
        // Sync left HEVC on (applied true) and now Jellyfin reads false: the user turned it off.
        let prev = OfferState {
            intent: true,
            applied: Some(true),
        };
        let (offer, state) = decide(
            "AllowHevcEncoding",
            "hevc",
            Some(false),
            true,
            &[],
            Some(&prev),
        );
        assert!(!offer.intent);
        assert!(!offer.effective);
        assert_eq!(offer.cause, "user_disabled");
        assert_eq!(
            state,
            OfferState {
                intent: false,
                applied: Some(false)
            }
        );
        // And the reverse: the user turns it back on.
        let prev = OfferState {
            intent: false,
            applied: Some(false),
        };
        let (offer, _) = decide(
            "AllowHevcEncoding",
            "hevc",
            Some(true),
            true,
            &[],
            Some(&prev),
        );
        assert!(offer.intent && offer.effective);
        assert_eq!(offer.cause, "enabled");
    }

    #[test]
    fn pool_cannot_names_every_worker_that_is_missing_the_output() {
        let (offer, _) = decide(
            "AllowHevcEncoding",
            "hevc",
            Some(true),
            false,
            &lacking_names(&["p4", "10.42.0.9:9901"]),
            None,
        );
        assert_eq!(offer.cause, "pool_cannot");
        assert_eq!(offer.reason, "p4, 10.42.0.9:9901 cannot output hevc");
    }

    #[test]
    fn lacking_counts_a_never_seen_worker_as_h264_only() {
        let mut known_map = BTreeMap::new();
        known_map.insert("arc".to_string(), known(&["h264", "hevc", "av1"]));
        // "new" has never reported, so it is assumed h264-only and blocks both offers.
        assert_eq!(
            lacking(&["arc".into(), "new".into()], &known_map, "hevc"),
            lacking_names(&["new"])
        );
        assert!(lacking(&["arc".into(), "new".into()], &known_map, "h264").is_empty());
    }

    #[test]
    fn offer_state_deserializes_from_a_caps_file_without_the_field() {
        // The gotcha this guards: a missing field failing the whole parse would silently wipe the
        // last-known outputs and regress every offer to h264-only.
        let old = r#"{"workers":{"arc":{"outputs":["h264","hevc"],"kind":"qsv","seen_unix":1,
            "capacity":14.0,"units_used":2.0}},"common":["h264","hevc"],"live":["arc"],
            "synced_unix":1}"#;
        let state: StateFile = serde_json::from_str(old).expect("old caps file still parses");
        assert_eq!(state.workers["arc"].capacity, 14.0);
        assert!(state.offers.is_empty());
        assert_eq!(state.workers["arc"].addr, "");
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
        // AllowHevcEncoding starts true: with no persisted OfferState this is read as the user's
        // own intent (the upgrade path -- see `decide`'s doc comment), so the pool being able to
        // do hevc keeps it effective=true (no POST needed for this option). AllowAv1Encoding
        // starts true (the user wants it) but the pool cannot do av1, so `decide` flips it to
        // false and a POST is required -- exercising the write path this test is named for.
        let enc =
            serde_json::json!({"AllowHevcEncoding": true, "AllowAv1Encoding": true, "Keep": 7});
        let (qsv, qsv_log) = fake_jellyfin(enc.clone());
        let (nvenc, nvenc_log) = fake_jellyfin(enc);
        let mut known_map = BTreeMap::new();
        known_map.insert("qsv".to_string(), known(&["h264", "hevc", "av1"]));
        known_map.insert("nvenc".to_string(), known(&["h264", "hevc"]));
        let cfg = vec!["qsv".to_string(), "nvenc".to_string()];
        let common = common(&cfg, &known_map);
        let (offers, _) = apply_offers(
            &[qsv, nvenc],
            "t",
            &common,
            &cfg,
            &known_map,
            &BTreeMap::new(),
        )
        .unwrap();
        let by_option: BTreeMap<&str, bool> =
            offers.iter().map(|o| (o.option, o.effective)).collect();
        assert!(by_option["AllowHevcEncoding"]);
        assert!(!by_option["AllowAv1Encoding"]);
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
        let mut known_map = BTreeMap::new();
        known_map.insert("qsv".to_string(), known(&["h264", "hevc"]));
        let cfg = vec!["qsv".to_string()];
        let err = apply_offers(
            &[down.clone(), up],
            "t",
            &["h264".into(), "hevc".into()],
            &cfg,
            &known_map,
            &BTreeMap::new(),
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
        std::env::set_var("JF_URL", &jf);
        let targets = vec![t("nvenc", &dead, "static")];
        let mut state = StateFile::default();
        let r = sync_once(
            &mut state,
            &targets,
            Duration::ZERO,
            Duration::from_secs(120),
        )
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
            &targets,
            Duration::from_secs(120),
            Duration::from_secs(120),
        )
        .await
        .unwrap()
        .unwrap();
        let hevc = r.iter().find(|o| o.option == "AllowHevcEncoding").unwrap();
        assert!(!hevc.effective);
        std::env::remove_var("JF_URL");
    }
}
