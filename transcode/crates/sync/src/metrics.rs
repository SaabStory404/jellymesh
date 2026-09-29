//! Prometheus text-format metrics for tcpool-sync, plus the routing for the `/status` JSON view
//! (crate::status). Same hand-rolled approach as the agent (crates/agent/src/metrics.rs): no
//! `prometheus` crate, a tiny renderer plus a bare `tokio::net::TcpListener` HTTP handler. Off
//! unless `TC_METRICS_PORT` is set; `/metrics` (and anything that is not `/status`) is the scrape
//! path, so an existing scraper is unaffected by the extra route.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::status::Offer;

/// Output tokens the agent probes for (crates/agent/src/probe.rs `OUTPUTS`); duplicated here
/// rather than pulling in the agent crate for five string literals.
pub const OUTPUT_TOKENS: [&str; 5] = ["h264", "hevc", "hevc10", "av1", "av1-10"];

#[derive(Clone, Default)]
pub struct WorkerInfo {
    pub addr: String,
    /// How this cycle found the worker: `dns`, `static` (a TC_WORKERS entry), or `persisted` (it
    /// is only in the caps file -- its address is no longer in DNS). `persisted` is the interesting
    /// one on a dashboard: it is why an offer can still be held down by a worker that is gone.
    pub source: String,
    pub kind: String,
    /// Last-known outputs; the assumed `UNKNOWN` (h264-only) for a worker that never reported.
    pub outputs: Vec<String>,
    pub capacity: f64,
    pub units_used: f64,
    pub live: bool,
    pub seen_unix: i64,
}

/// What the metrics and `/status` handlers render; refreshed after every sync cycle in `main`.
#[derive(Clone, Default)]
pub struct Snapshot {
    /// Every worker the intersection is computed over: this cycle's discovered targets plus the
    /// workers persisted from earlier cycles. Down and never-seen workers are included
    /// (live=false) so `tcpool_pool_worker_live` and the status page always have a series for
    /// every worker that is holding an offer down, not just the ones that have reported.
    pub workers_configured: usize,
    pub workers: BTreeMap<String, WorkerInfo>,
    pub common: Vec<String>,
    pub synced_unix: i64,
    pub last_success_unix: i64,
    pub offers: Vec<Offer>,
}

pub type Shared = Arc<Mutex<Snapshot>>;

/// "Could the pool absorb losing its single biggest live card right now?" A pure function over
/// (capacity, units_used) pairs of only the LIVE workers -- down workers already contribute zero
/// capacity, so including them would double-penalize. A pool with no live workers is never
/// survivable, even though the literal `total - max >= used` arithmetic gives `0 - 0 >= 0 = true`
/// for that degenerate case.
pub fn survivable(live: &[(f64, f64)]) -> bool {
    if live.is_empty() {
        return false;
    }
    let total_cap: f64 = live.iter().map(|(c, _)| c).sum();
    let max_cap: f64 = live.iter().map(|(c, _)| *c).fold(0.0, f64::max);
    let total_used: f64 = live.iter().map(|(_, u)| u).sum();
    total_cap - max_cap >= total_used - 1e-9
}

fn escape(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn fmt_f64(v: f64) -> String {
    if v.is_nan() {
        "NaN".to_string()
    } else if v.is_infinite() {
        if v > 0.0 {
            "+Inf".to_string()
        } else {
            "-Inf".to_string()
        }
    } else {
        let mut s = format!("{v}");
        if s.ends_with(".0") {
            s.truncate(s.len() - 2);
        }
        s
    }
}

pub fn render(snap: &Snapshot) -> String {
    let mut out = String::new();

    let _ = writeln!(out, "# TYPE tcpool_pool_common_output gauge");
    for token in OUTPUT_TOKENS {
        let v = snap.common.iter().any(|c| c == token) as u8;
        let _ = writeln!(
            out,
            "tcpool_pool_common_output{{output=\"{}\"}} {v}",
            escape(token)
        );
    }

    let _ = writeln!(out, "# TYPE tcpool_pool_workers_configured gauge");
    let _ = writeln!(
        out,
        "tcpool_pool_workers_configured {}",
        snap.workers_configured
    );

    let live_count = snap.workers.values().filter(|w| w.live).count();
    let _ = writeln!(out, "# TYPE tcpool_pool_workers_live gauge");
    let _ = writeln!(out, "tcpool_pool_workers_live {live_count}");

    // Per-worker liveness: TranscodeWorkerDown needs to name which worker is down, which a total
    // (workers_live) can't do.
    let _ = writeln!(out, "# TYPE tcpool_pool_worker_live gauge");
    for (name, w) in &snap.workers {
        let _ = writeln!(
            out,
            "tcpool_pool_worker_live{{worker=\"{}\"}} {}",
            escape(name),
            w.live as u8
        );
    }

    let _ = writeln!(out, "# TYPE tcpool_pool_capacity_units gauge");
    for (name, w) in &snap.workers {
        let _ = writeln!(
            out,
            "tcpool_pool_capacity_units{{worker=\"{}\"}} {}",
            escape(name),
            fmt_f64(w.capacity)
        );
    }

    let live_pairs: Vec<(f64, f64)> = snap
        .workers
        .values()
        .filter(|w| w.live)
        .map(|w| (w.capacity, w.units_used))
        .collect();
    let _ = writeln!(out, "# TYPE tcpool_pool_survivable gauge");
    let _ = writeln!(
        out,
        "tcpool_pool_survivable {}",
        survivable(&live_pairs) as u8
    );

    let _ = writeln!(
        out,
        "# TYPE tcpool_sync_last_success_timestamp_seconds gauge"
    );
    let _ = writeln!(
        out,
        "tcpool_sync_last_success_timestamp_seconds {}",
        snap.last_success_unix
    );

    // `tcpool_jellyfin_offer` stays what it always was -- the value sync left in Jellyfin's config,
    // i.e. what Jellyfin actually offers. `_intent` is the operator's checkbox, so the gap between
    // the two is exactly "the pool can't", which /status spells out per offer with a reason.
    let _ = writeln!(out, "# TYPE tcpool_jellyfin_offer gauge");
    for offer in &snap.offers {
        let _ = writeln!(
            out,
            "tcpool_jellyfin_offer{{option=\"{}\"}} {}",
            escape(offer.option),
            offer.effective as u8
        );
    }
    let _ = writeln!(out, "# TYPE tcpool_jellyfin_offer_intent gauge");
    for offer in &snap.offers {
        let _ = writeln!(
            out,
            "tcpool_jellyfin_offer_intent{{option=\"{}\"}} {}",
            escape(offer.option),
            offer.intent as u8
        );
    }

    out
}

/// Serve Prometheus metrics and `/status` on `port` until the process exits. Binding failure exits
/// the process: k8s expects this port once `TC_METRICS_PORT` is set.
pub async fn serve(shared: Shared, port: u16) {
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("tcpool-sync: metrics server on :{port}: {e}");
            std::process::exit(1);
        }
    };
    crate::log(&format!(
        "metrics on :{port} (text/plain; version=0.0.4) and /status (application/json)"
    ));
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(_) => continue,
        };
        let shared = shared.clone();
        tokio::spawn(handle_conn(stream, shared));
    }
}

/// The request path from a request head, or None if there is no complete request line in it.
/// Split out from the socket loop so it can be tested without a connection.
fn request_path(head: &[u8]) -> Option<String> {
    let line = head.split(|b| *b == b'\n').next()?;
    let mut parts = line.split(|b| *b == b' ').filter(|p| !p.is_empty());
    parts.next()?; // method
    let target = parts.next()?; // origin-form target
    let target = String::from_utf8_lossy(target);
    Some(
        target
            .split(['?', '#'])
            .next()
            .unwrap_or("")
            .trim()
            .to_string(),
    )
}

const MAX_REQUEST_HEAD: usize = 1024;

/// Read up to the first line of the request head. Bounded and tolerant: the request line is first
/// and tiny, and if the client says nothing we serve metrics anyway (the scrape path), so a
/// scraper that trips over a partial request still gets a body -- as it did before /status existed.
async fn read_path(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut head: Vec<u8> = Vec::with_capacity(MAX_REQUEST_HEAD);
    let mut chunk = [0u8; 256];
    while !head.contains(&b'\n') && head.len() < MAX_REQUEST_HEAD {
        match tokio::time::timeout(Duration::from_millis(500), stream.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => head.extend_from_slice(&chunk[..n]),
        }
    }
    request_path(&head)
}

async fn handle_conn(mut stream: tokio::net::TcpStream, shared: Shared) {
    let path = read_path(&mut stream).await;
    let snap = {
        let snap = shared.lock().unwrap_or_else(|p| p.into_inner());
        snap.clone()
    };
    let (content_type, body) = if path.as_deref() == Some("/status") {
        (
            "application/json",
            crate::status::render_json(&snap, crate::now_unix()),
        )
    } else {
        ("text/plain; version=0.0.4", render(&snap))
    };
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(option: &'static str, intent: bool, effective: bool) -> Offer {
        Offer {
            option,
            token: "hevc",
            intent,
            effective,
            cause: "pool_cannot",
            reason: "p4 cannot output hevc".into(),
            lacking: vec!["p4".into()],
        }
    }

    #[test]
    fn survivable_two_equal_workers_half_loaded() {
        // 10 units each, 10 used total: losing either leaves 10 capacity for 10 used -> ok.
        assert!(survivable(&[(10.0, 5.0), (10.0, 5.0)]));
    }

    #[test]
    fn survivable_one_worker_carrying_load_is_not() {
        // Losing the P4 (the only other worker) leaves 0 capacity while the Arc still has jobs.
        assert!(!survivable(&[(5.0, 2.0), (3.0, 3.0)]));
    }

    #[test]
    fn survivable_no_live_workers_is_false_not_vacuously_true() {
        assert!(!survivable(&[]));
    }

    #[test]
    fn survivable_exact_boundary_is_ok() {
        // total 10, max 6 -> remaining 4; used exactly 4 -> still survivable (>=).
        assert!(survivable(&[(6.0, 3.0), (4.0, 1.0)]));
    }

    #[test]
    fn render_reports_every_known_output_token() {
        let mut snap = Snapshot {
            common: vec!["h264".into(), "hevc".into()],
            ..Default::default()
        };
        snap.workers_configured = 2;
        let text = render(&snap);
        assert!(text.contains("tcpool_pool_common_output{output=\"h264\"} 1"));
        assert!(text.contains("tcpool_pool_common_output{output=\"hevc\"} 1"));
        assert!(text.contains("tcpool_pool_common_output{output=\"hevc10\"} 0"));
        assert!(text.contains("tcpool_pool_common_output{output=\"av1\"} 0"));
        assert!(text.contains("tcpool_pool_common_output{output=\"av1-10\"} 0"));
        assert!(text.contains("tcpool_pool_workers_configured 2"));
    }

    #[test]
    fn render_survivable_reflects_only_live_workers() {
        let mut workers = BTreeMap::new();
        workers.insert(
            "arc".into(),
            WorkerInfo {
                capacity: 10.0,
                units_used: 8.0,
                live: true,
                ..Default::default()
            },
        );
        workers.insert(
            "p4".into(),
            WorkerInfo {
                capacity: 5.0,
                units_used: 0.0,
                live: false, // down: must not count toward capacity
                ..Default::default()
            },
        );
        let snap = Snapshot {
            workers,
            ..Default::default()
        };
        // Only "arc" is live; losing it (the biggest, and only, live card) leaves 0 for 8 used.
        assert!(render(&snap).contains("tcpool_pool_survivable 0"));
    }

    #[test]
    fn render_separates_intent_from_effective() {
        let snap = Snapshot {
            offers: vec![offer("AllowHevcEncoding", true, false)],
            ..Default::default()
        };
        let text = render(&snap);
        // Jellyfin does not offer it (what the pool left in the config)...
        assert!(text.contains("tcpool_jellyfin_offer{option=\"AllowHevcEncoding\"} 0"));
        // ...but the operator asked for it, so the pair is visible without /status.
        assert!(text.contains("tcpool_jellyfin_offer_intent{option=\"AllowHevcEncoding\"} 1"));
    }

    #[test]
    fn request_path_routes_status_and_defaults_to_metrics() {
        assert_eq!(
            request_path(b"GET /status HTTP/1.1\r\nHost: x\r\n\r\n").as_deref(),
            Some("/status")
        );
        assert_eq!(
            request_path(b"GET /status?pretty=1 HTTP/1.1\r\n").as_deref(),
            Some("/status")
        );
        assert_eq!(
            request_path(b"GET /metrics HTTP/1.1\r\n").as_deref(),
            Some("/metrics")
        );
        // A path we don't route on must fall through to the scrape body, not 404.
        assert_eq!(request_path(b"GET / HTTP/1.1\r\n").as_deref(), Some("/"));
        // Nothing usable yet: no request line, or only a partial one.
        assert_eq!(request_path(b""), None);
        assert_eq!(request_path(b"\r\n"), None);
        assert_eq!(request_path(b"GET"), None);
        assert_eq!(request_path(b"GET /sta"), Some("/sta".to_string()));
    }
}
