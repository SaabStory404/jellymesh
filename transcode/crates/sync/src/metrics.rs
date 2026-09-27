//! Prometheus text-format metrics for tcpool-sync. Same hand-rolled approach as the agent
//! (crates/agent/src/metrics.rs): no `prometheus` crate, a tiny renderer plus a bare
//! `tokio::net::TcpListener` HTTP handler. Off unless `TC_METRICS_PORT` is set.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Output tokens the agent probes for (crates/agent/src/probe.rs `OUTPUTS`); duplicated here
/// rather than pulling in the agent crate for five string literals.
pub const OUTPUT_TOKENS: [&str; 5] = ["h264", "hevc", "hevc10", "av1", "av1-10"];

#[derive(Clone, Default)]
pub struct WorkerInfo {
    pub capacity: f64,
    pub units_used: f64,
    pub live: bool,
}

/// What the metrics endpoint renders; refreshed after every sync cycle in `main`.
#[derive(Clone, Default)]
pub struct Snapshot {
    pub workers_configured: usize,
    /// Keyed by configured worker name; includes down/never-seen workers (live=false,
    /// capacity=0 until first Hello) so `tcpool_pool_worker_live` always has a series to alert
    /// on for every configured worker, not just ones that have reported.
    pub workers: BTreeMap<String, WorkerInfo>,
    pub common: Vec<String>,
    pub last_success_unix: i64,
    pub offers: Vec<(&'static str, bool)>,
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

    // Per-worker liveness: TranscodeWorkerDown needs to name which configured worker is down,
    // which a total (workers_live) can't do.
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

    let _ = writeln!(out, "# TYPE tcpool_jellyfin_offer gauge");
    for (option, want) in &snap.offers {
        let _ = writeln!(
            out,
            "tcpool_jellyfin_offer{{option=\"{}\"}} {}",
            escape(option),
            *want as u8
        );
    }

    out
}

/// Serve the Prometheus exposition on `port` until the process exits. Binding failure exits the
/// process: k8s expects this port once `TC_METRICS_PORT` is set.
pub async fn serve(shared: Shared, port: u16) {
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("tcpool-sync: metrics server on :{port}: {e}");
            std::process::exit(1);
        }
    };
    crate::log(&format!("metrics on :{port} (text/plain; version=0.0.4)"));
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(_) => continue,
        };
        let shared = shared.clone();
        tokio::spawn(handle_conn(stream, shared));
    }
}

async fn handle_conn(mut stream: tokio::net::TcpStream, shared: Shared) {
    // See the agent's metrics::handle_conn: one bounded, unparsed read is enough here too.
    let mut buf = [0u8; 1024];
    let _ = tokio::time::timeout(Duration::from_millis(500), stream.read(&mut buf)).await;
    let body = {
        let snap = shared.lock().unwrap_or_else(|p| p.into_inner());
        render(&snap)
    };
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

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
            },
        );
        workers.insert(
            "p4".into(),
            WorkerInfo {
                capacity: 5.0,
                units_used: 0.0,
                live: false, // down: must not count toward capacity
            },
        );
        let snap = Snapshot {
            workers,
            ..Default::default()
        };
        // Only "arc" is live; losing it (the biggest, and only, live card) leaves 0 for 8 used.
        assert!(render(&snap).contains("tcpool_pool_survivable 0"));
    }
}
