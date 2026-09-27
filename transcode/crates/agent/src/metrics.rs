//! Prometheus text-format metrics for tcpool-agent. No dependency on the `prometheus` crate
//! (would need to build for musl): a tiny renderer over atomics/mutexes plus a bare
//! `tokio::net::TcpListener` HTTP handler. Off unless `TC_METRICS_PORT` is set.

use crate::probe;
use crate::State;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Why a job's lifecycle event happened. Every value is always rendered (even at 0) so
/// `increase()`/`rate()` work from the very first scrape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Accepted,
    Busy,
    RefusedPolicy,
    ExitOk,
    ExitError,
    Fenced,
    Stalled,
    Drained,
    GpuFilterFallback,
    /// A BATCH job's `reserve_batch` refused because admitting it would have eaten into
    /// `TC_BATCH_HEADROOM` (distinct from `Busy`'s `"capacity"`/`"draining"`/`"batch-disabled"`
    /// reasons, which are not headroom-specific).
    BusyHeadroom,
    /// A BATCH job ended because a PLAYBACK admission preempted it (before or after it produced
    /// any output).
    Preempted,
    /// A BATCH job was admitted (counted in addition to, not instead of, `Accepted`).
    BatchAccepted,
}

impl Outcome {
    pub const ALL: [Outcome; 12] = [
        Outcome::Accepted,
        Outcome::Busy,
        Outcome::RefusedPolicy,
        Outcome::ExitOk,
        Outcome::ExitError,
        Outcome::Fenced,
        Outcome::Stalled,
        Outcome::Drained,
        Outcome::GpuFilterFallback,
        Outcome::BusyHeadroom,
        Outcome::Preempted,
        Outcome::BatchAccepted,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Accepted => "accepted",
            Outcome::Busy => "busy",
            Outcome::RefusedPolicy => "refused_policy",
            Outcome::ExitOk => "exit_ok",
            Outcome::ExitError => "exit_error",
            Outcome::Fenced => "fenced",
            Outcome::Stalled => "stalled",
            Outcome::Drained => "drained",
            Outcome::GpuFilterFallback => "gpu_filter_fallback",
            Outcome::BusyHeadroom => "busy_headroom",
            Outcome::Preempted => "preempted",
            Outcome::BatchAccepted => "batch_accepted",
        }
    }
}

/// Wall-time buckets (seconds) for `tcpool_job_seconds`, sized for transcode sessions:
/// a few seconds (probe/copy) through a multi-hour movie.
const DURATION_BUCKETS: [f64; 9] = [
    10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0, 7200.0,
];

/// A tiny cumulative histogram. `observe()` happens once per job (job completion), so a mutex
/// around the sum/count is cheap; the hotter per-scrape read path only ever loads atomics.
struct Histogram {
    buckets: &'static [f64],
    /// Per-bucket (non-cumulative) counts; last slot is the +Inf overflow bucket.
    counts: Vec<AtomicU64>,
    sum: Mutex<f64>,
    count: AtomicU64,
}

impl Histogram {
    fn new(buckets: &'static [f64]) -> Self {
        let mut counts = Vec::with_capacity(buckets.len() + 1);
        counts.resize_with(buckets.len() + 1, || AtomicU64::new(0));
        Histogram {
            buckets,
            counts,
            sum: Mutex::new(0.0),
            count: AtomicU64::new(0),
        }
    }

    fn observe(&self, v: f64) {
        let idx = self
            .buckets
            .iter()
            .position(|b| v <= *b)
            .unwrap_or(self.buckets.len());
        self.counts[idx].fetch_add(1, Ordering::Relaxed);
        *self.sum.lock().unwrap_or_else(|p| p.into_inner()) += v;
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    fn render(&self, name: &str, base_labels: &str, out: &mut String) {
        let _ = writeln!(out, "# TYPE {name} histogram");
        let mut cumulative = 0u64;
        for (i, b) in self.buckets.iter().enumerate() {
            cumulative += self.counts[i].load(Ordering::Relaxed);
            let _ = writeln!(
                out,
                "{name}_bucket{{{base_labels},le=\"{}\"}} {cumulative}",
                fmt_f64(*b)
            );
        }
        cumulative += self.counts[self.buckets.len()].load(Ordering::Relaxed);
        let _ = writeln!(
            out,
            "{name}_bucket{{{base_labels},le=\"+Inf\"}} {cumulative}"
        );
        let sum = *self.sum.lock().unwrap_or_else(|p| p.into_inner());
        let _ = writeln!(out, "{name}_sum{{{base_labels}}} {}", fmt_f64(sum));
        let _ = writeln!(out, "{name}_count{{{base_labels}}} {cumulative}");
    }
}

/// Metrics carried on `State`, updated at job-lifecycle events (not on the ffmpeg stdout/stdin
/// hot path). Speed samples come from parsing ffmpeg stderr, which already happens per chunk in
/// job.rs; only the value that changed is written here.
pub struct Metrics {
    jobs_total: [AtomicU64; Outcome::ALL.len()],
    job_seconds: Histogram,
    /// job id -> last observed `speed=` factor, while the job is running and not paused.
    job_speed: Mutex<HashMap<u64, f64>>,
    next_job_id: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Metrics {
            jobs_total: std::array::from_fn(|_| AtomicU64::new(0)),
            job_seconds: Histogram::new(&DURATION_BUCKETS),
            job_speed: Mutex::new(HashMap::new()),
            next_job_id: AtomicU64::new(1),
        }
    }
}

impl Metrics {
    pub fn inc(&self, outcome: Outcome) {
        self.jobs_total[outcome as usize].fetch_add(1, Ordering::Relaxed);
    }

    pub fn new_job_id(&self) -> u64 {
        self.next_job_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn observe_seconds(&self, secs: f64) {
        self.job_seconds.observe(secs);
    }

    /// Record the last realtime factor for a running job. Jellyfin's throttler pauses ffmpeg
    /// once it's ~60s ahead, at which point `speed=` freezes near/at 1.0x; a paused job is not
    /// slow, so the caller must not call this while paused (see job.rs `Ctl::note_stderr`).
    pub fn set_speed(&self, job_id: u64, speed: f64) {
        self.job_speed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(job_id, speed);
    }

    /// Stop reporting a speed for this job: on pause (not a stall) or job end.
    pub fn clear_speed(&self, job_id: u64) {
        self.job_speed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&job_id);
    }
}

/// Parse ffmpeg's `speed=%4.3gx` (e.g. `speed=   1x`, `speed=1.23x`, `speed=N/A`) out of a
/// stderr chunk. ffmpeg pads the value with leading spaces and suffixes `x`; `time=` parsing
/// nearby gets away without trimming because it's a different, fixed-width field.
pub fn parse_speed(text: &str) -> Option<f64> {
    let tail = text.rsplit("speed=").next()?;
    let token: String = tail
        .trim_start()
        .chars()
        .take_while(|c| !c.is_whitespace())
        .collect();
    let token = token.strip_suffix('x').unwrap_or(&token);
    if token.is_empty() || token == "N/A" {
        return None;
    }
    token.parse::<f64>().ok()
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
        // Prometheus accepts plain decimal; trim noise from e.g. 3.0 -> "3".
        let mut s = format!("{v}");
        if s.ends_with(".0") {
            s.truncate(s.len() - 2);
        }
        s
    }
}

fn base_labels(state: &State) -> String {
    format!(
        "worker=\"{}\",kind=\"{}\",node=\"{}\"",
        escape(&state.cfg.name),
        escape(state.cfg.backend.as_str()),
        escape(&state.cfg.node),
    )
}

/// Render the full Prometheus text-format exposition for this agent.
pub fn render(state: &State) -> String {
    let labels = base_labels(state);
    let mut out = String::new();

    let (used, cap) = state.usage.snapshot();

    let _ = writeln!(out, "# TYPE tcpool_capacity_units gauge");
    let _ = writeln!(out, "tcpool_capacity_units{{{labels}}} {}", fmt_f64(cap));

    let _ = writeln!(out, "# TYPE tcpool_units_used gauge");
    let _ = writeln!(out, "tcpool_units_used{{{labels}}} {}", fmt_f64(used));

    let _ = writeln!(out, "# TYPE tcpool_jobs_active gauge");
    let _ = writeln!(out, "tcpool_jobs_active{{{labels}}} {}", state.usage.jobs());

    let (batch_used, batch_jobs) = state.usage.batch_snapshot();
    let _ = writeln!(out, "# TYPE tcpool_batch_units_used gauge");
    let _ = writeln!(
        out,
        "tcpool_batch_units_used{{{labels}}} {}",
        fmt_f64(batch_used)
    );
    let _ = writeln!(out, "# TYPE tcpool_batch_jobs_active gauge");
    let _ = writeln!(out, "tcpool_batch_jobs_active{{{labels}}} {batch_jobs}");
    let _ = writeln!(out, "# TYPE tcpool_batch_headroom_units gauge");
    let _ = writeln!(
        out,
        "tcpool_batch_headroom_units{{{labels}}} {}",
        fmt_f64(state.usage.headroom())
    );

    let _ = writeln!(out, "# TYPE tcpool_jobs_total counter");
    for outcome in Outcome::ALL {
        let n = state.metrics.jobs_total[outcome as usize].load(Ordering::Relaxed);
        let _ = writeln!(
            out,
            "tcpool_jobs_total{{{labels},outcome=\"{}\"}} {n}",
            outcome.as_str()
        );
    }

    state
        .metrics
        .job_seconds
        .render("tcpool_job_seconds", &labels, &mut out);

    let _ = writeln!(out, "# TYPE tcpool_job_speed gauge");
    {
        let speeds = state
            .metrics
            .job_speed
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut ids: Vec<_> = speeds.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            let v = speeds[&id];
            let _ = writeln!(
                out,
                "tcpool_job_speed{{{labels},job=\"{id}\"}} {}",
                fmt_f64(v)
            );
        }
    }

    let _ = writeln!(out, "# TYPE tcpool_probe_output gauge");
    for (token, _, _) in probe::OUTPUTS {
        let v = if state.probed.outputs.iter().any(|o| o == token) {
            1
        } else {
            0
        };
        let _ = writeln!(
            out,
            "tcpool_probe_output{{{labels},output=\"{token}\"}} {v}"
        );
    }

    let _ = writeln!(out, "# TYPE tcpool_gpu_tonemap gauge");
    let _ = writeln!(
        out,
        "tcpool_gpu_tonemap{{{labels}}} {}",
        state.probed.gpu_tonemap as u8
    );

    let _ = writeln!(out, "# TYPE tcpool_draining gauge");
    let _ = writeln!(
        out,
        "tcpool_draining{{{labels}}} {}",
        *state.drain.borrow() as u8
    );

    let _ = writeln!(out, "# TYPE tcpool_build_info gauge");
    let _ = writeln!(
        out,
        "tcpool_build_info{{{labels},version=\"{}\",ffmpeg=\"{}\"}} 1",
        escape(env!("CARGO_PKG_VERSION")),
        escape(&state.probed.ffmpeg_version),
    );

    out
}

/// Serve the Prometheus exposition on `port` until the process exits. Binding failure exits the
/// process (like the health server): k8s expects this port once `TC_METRICS_PORT` is set.
pub async fn serve(state: std::sync::Arc<State>, port: u16) {
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("tcpool-agent: metrics server on :{port}: {e}");
            std::process::exit(1);
        }
    };
    crate::log(format_args!(
        "metrics on :{port} (text/plain; version=0.0.4)"
    ));
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(_) => continue,
        };
        let state = state.clone();
        tokio::spawn(handle_conn(stream, state));
    }
}

async fn handle_conn(mut stream: tokio::net::TcpStream, state: std::sync::Arc<State>) {
    // Drain (but don't require completing) the request head with a short grace window, in case
    // the scraper's client hasn't finished writing it before we're scheduled. We don't parse the
    // request at all -- any GET on any path gets the same body -- so one bounded read is enough;
    // there's nothing to loop for.
    let mut buf = [0u8; 1024];
    let _ = tokio::time::timeout(Duration::from_millis(500), stream.read(&mut buf)).await;
    let body = render(&state);
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
    fn speed_parses_common_shapes() {
        assert_eq!(parse_speed("frame=1 speed=   1x"), Some(1.0));
        assert_eq!(parse_speed("frame=1 speed=1.23x"), Some(1.23));
        assert_eq!(parse_speed("frame=1 speed=0.5x    \n"), Some(0.5));
        assert_eq!(parse_speed("frame=1 speed=N/A"), None);
        assert_eq!(parse_speed("frame=1 no speed here"), None);
        // last occurrence wins, matching how note_stderr picks time=
        assert_eq!(parse_speed("speed=2x junk speed=3x"), Some(3.0));
    }

    #[test]
    fn escape_handles_quotes_backslashes_newlines() {
        assert_eq!(escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    #[test]
    fn fmt_f64_trims_trailing_zero() {
        assert_eq!(fmt_f64(3.0), "3");
        assert_eq!(fmt_f64(3.5), "3.5");
        assert_eq!(fmt_f64(f64::INFINITY), "+Inf");
    }

    #[test]
    fn histogram_buckets_are_cumulative() {
        let h = Histogram::new(&[1.0, 5.0, 10.0]);
        h.observe(0.5);
        h.observe(3.0);
        h.observe(3.0);
        h.observe(20.0);
        let mut out = String::new();
        h.render("x", "w=\"a\"", &mut out);
        assert!(out.contains("x_bucket{w=\"a\",le=\"1\"} 1"));
        assert!(out.contains("x_bucket{w=\"a\",le=\"5\"} 3"));
        assert!(out.contains("x_bucket{w=\"a\",le=\"10\"} 3"));
        assert!(out.contains("x_bucket{w=\"a\",le=\"+Inf\"} 4"));
        assert!(out.contains("x_count{w=\"a\"} 4"));
    }

    #[test]
    fn jobs_total_renders_every_outcome_at_zero() {
        let m = Metrics::default();
        m.inc(Outcome::Accepted);
        m.inc(Outcome::Accepted);
        m.inc(Outcome::ExitOk);
        m.inc(Outcome::BusyHeadroom);
        m.inc(Outcome::Preempted);
        m.inc(Outcome::Preempted);
        m.inc(Outcome::BatchAccepted);
        for outcome in Outcome::ALL {
            let n = m.jobs_total[outcome as usize].load(Ordering::Relaxed);
            match outcome {
                Outcome::Accepted => assert_eq!(n, 2),
                Outcome::ExitOk => assert_eq!(n, 1),
                Outcome::BusyHeadroom => assert_eq!(n, 1),
                Outcome::Preempted => assert_eq!(n, 2),
                Outcome::BatchAccepted => assert_eq!(n, 1),
                _ => assert_eq!(n, 0),
            }
        }
    }

    #[test]
    fn job_speed_set_and_clear() {
        let m = Metrics::default();
        let id = m.new_job_id();
        m.set_speed(id, 1.5);
        assert_eq!(*m.job_speed.lock().unwrap().get(&id).unwrap(), 1.5);
        m.clear_speed(id);
        assert!(m.job_speed.lock().unwrap().get(&id).is_none());
    }
}
