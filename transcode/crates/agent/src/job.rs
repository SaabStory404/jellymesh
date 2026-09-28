//! One job: admission, ffmpeg lifecycle, heartbeats, fencing, and the CPU-filter re-run.

use crate::config::Config;
use crate::probe::source_video;
use crate::State;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tcpool_ir::{
    add_dv_removal_bsf, first_segment, input_path, is_video_copy, map_path, render,
    render_trickplay, strip_dv81_signal, wants_dv81, Shape, SourceVideo, TranslateOpts,
};
use tcpool_proto::{
    client_msg, server_msg, Accepted, Busy, ClientMsg, Exit, Heartbeat, Job, ServerMsg,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch, Mutex};
use tonic::{Status, Streaming};

type Tx = mpsc::Sender<Result<ServerMsg, Status>>;

fn msg(m: server_msg::Msg) -> Result<ServerMsg, Status> {
    Ok(ServerMsg { msg: Some(m) })
}

/// Units this job costs on this card, and what the (single) admission ffprobe saw of the source
/// (reused by the P5.1 bitrate ladder in `render()`).
async fn job_weight(cfg: &Config, args: &[String]) -> (f64, Option<SourceVideo>) {
    if is_video_copy(args) {
        return (cfg.weight_copy, None);
    }
    let input = input_path(args).map(|p| map_path(p, &cfg.pathmap));
    let source = match input {
        Some(p) => source_video(cfg, &p).await,
        None => None,
    };
    (cfg.weight_for_height(source.map(|s| s.height)), source)
}

/// Why the agent ended a job itself (not fencing).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ended {
    Stalled,
    Drained,
    /// A PLAYBACK admission preempted this BATCH job (before or after it produced any output).
    Preempted,
}

/// Why a BATCH job is refused outright before admission is even attempted, or `None` to proceed.
/// A pure function (config in, verdict out) so it's directly unit-testable without a live
/// gRPC/ffmpeg harness.
fn batch_refusal_reason(cfg: &Config, batch: bool) -> Option<&'static str> {
    if !batch {
        return None;
    }
    if !cfg.accept_batch {
        return Some("batch-disabled"); // A5: this worker opted out of BATCH entirely
    }
    if cfg.policy.trickplay_output_root.is_none() {
        return Some("batch-disabled"); // A2: an unset root refuses BATCH the same way
    }
    None
}

/// The job's final `(metrics outcome, Exit.preempted)`, from ffmpeg's own exit code, whether the
/// agent fenced it, and whatever `Ended` reason a watchdog recorded (if any). `code == 0` (and
/// not fenced) proves ffmpeg finished on its own: every watchdog's kill is a SIGKILL via
/// `Ctl::end`/`Ctl::fence`, and a killed process can never report exit code 0. Checking that
/// first, ahead of `ended`, makes the result independent of a real but narrow race: `run_ffmpeg`
/// has one more await after `child.wait()` resolves (draining the stdout/stderr pump tasks,
/// bounded by a 2s timeout), and a watchdog poll (stall/drain/preempt) landing in that window can
/// still record an `Ended` reason for a job whose ffmpeg had, in truth, already exited 0. Without
/// this, e.g. `preempt_watch` recording `Ended::Preempted` in that window would mislabel an
/// already-successful BATCH job as preempted, causing the shim to discard good output and rerun
/// locally, and the metrics/alerting to record a false preemption.
fn final_outcome(code: i32, fenced: bool, ended: Option<Ended>) -> (crate::metrics::Outcome, bool) {
    if fenced {
        return (crate::metrics::Outcome::Fenced, false);
    }
    if code == 0 {
        return (crate::metrics::Outcome::ExitOk, false);
    }
    match ended {
        Some(Ended::Stalled) => (crate::metrics::Outcome::Stalled, false),
        // Drained jobs are killed to end them (a nonzero/signal exit code), but that's a
        // planned handoff to another worker, not a failure -- keep it out of exit_error.
        Some(Ended::Drained) => (crate::metrics::Outcome::Drained, false),
        Some(Ended::Preempted) => (crate::metrics::Outcome::Preempted, true),
        None => (crate::metrics::Outcome::ExitError, false),
    }
}

/// Shared between the tasks of one job.
struct Ctl {
    fenced: AtomicBool,
    done: AtomicBool,
    last_heard: Mutex<Instant>,
    kill: watch::Sender<bool>,
    /// Jellyfin's throttler paused ffmpeg (`p`); resumed with `u`. No progress is expected.
    paused: AtomicBool,
    /// Jellyfin asked ffmpeg to quit (`q`).
    quitting: AtomicBool,
    /// Last `time=` value ffmpeg reported, when it last changed, and whether any was seen.
    progress: std::sync::Mutex<(Instant, String, bool)>,
    ended: std::sync::Mutex<Option<Ended>>,
    /// Kept only to reach `state.metrics` from `note_stderr`; a cheap Arc clone.
    state: Arc<State>,
    job_id: u64,
}

impl Ctl {
    fn fence(&self, why: &str) {
        if !self.done.load(Ordering::SeqCst) && !self.fenced.swap(true, Ordering::SeqCst) {
            crate::log(format_args!("fencing: {why}; killing ffmpeg"));
            let _ = self.kill.send(true);
        }
    }

    /// End the job ourselves. Unlike fencing, the shim still gets an `exit`, so Jellyfin
    /// restarts the session at the next missing segment (on another worker).
    fn end(&self, why: Ended, detail: &str) {
        let mut e = self.ended.lock().unwrap_or_else(|p| p.into_inner());
        if e.is_none() && !self.done.load(Ordering::SeqCst) {
            *e = Some(why);
            crate::log(format_args!(
                "ending job ({why:?}): {detail}; killing ffmpeg"
            ));
            let _ = self.kill.send(true);
        }
    }

    fn ended(&self) -> Option<Ended> {
        *self.ended.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn note_stderr(&self, chunk: &[u8]) {
        let text = String::from_utf8_lossy(chunk);
        if text.contains("time=") {
            if let Some(t) = text.rsplit("time=").next() {
                let v: String = t.chars().take_while(|c| !c.is_whitespace()).collect();
                if !v.is_empty() && v != "N/A" {
                    let mut p = self.progress.lock().unwrap_or_else(|p| p.into_inner());
                    if p.1 != v {
                        *p = (Instant::now(), v, true);
                    }
                }
            }
        }
        // `speed=` is cumulative pts/wall time since start, so it settles near 1.0x once
        // Jellyfin's throttler is holding the encode back (see production plan §3) -- that's
        // healthy, not slow. Stop reporting a speed for this job while paused so a "speed < 1.0"
        // alert doesn't fire on a throttled-but-fine session; resume reporting once it resumes.
        if text.contains("speed=") {
            if self.paused.load(Ordering::SeqCst) {
                self.state.metrics.clear_speed(self.job_id);
            } else if let Some(speed) = crate::metrics::parse_speed(&text) {
                self.state.metrics.set_speed(self.job_id, speed);
            }
        }
    }
}

pub async fn run_job(
    state: Arc<State>,
    job: Job,
    shape: Shape,
    mut inbound: Streaming<ClientMsg>,
    tx: Tx,
) {
    let cfg = &state.cfg;
    // Shape (validated up front in main.rs's Svc::run), not the client-asserted `job.priority`,
    // decides admission: it can't be spoofed independently of the argv that was already checked.
    let batch = matches!(shape, Shape::Trickplay);
    let class = if batch { "batch" } else { "playback" };
    if let Some(reason) = batch_refusal_reason(cfg, batch) {
        let (used, cap) = state.usage.snapshot();
        crate::log(format_args!("busy ({reason}): refusing a batch job"));
        state.metrics.inc(crate::metrics::Outcome::Busy);
        let _ = tx
            .send(msg(server_msg::Msg::Busy(Busy {
                reason: reason.into(),
                units_used: used,
                capacity: cap,
            })))
            .await;
        return;
    }
    if *state.drain.borrow() {
        let (used, cap) = state.usage.snapshot();
        state.metrics.inc(crate::metrics::Outcome::Busy);
        let _ = tx
            .send(msg(server_msg::Msg::Busy(Busy {
                reason: "draining".into(),
                units_used: used,
                capacity: cap,
            })))
            .await;
        return;
    }
    // A3: BATCH uses a fixed weight and skips job_weight()'s ffprobe before admission.
    let (weight, source) = if batch {
        (cfg.batch_weight, None)
    } else {
        job_weight(cfg, &job.args).await
    };
    let guard = if batch {
        state.usage.reserve_batch(weight)
    } else {
        state.usage.reserve_playback(weight)
    };
    let Some(guard) = guard else {
        let (used, cap) = state.usage.snapshot();
        let reason = if batch { "headroom" } else { "capacity" };
        crate::log(format_args!(
            "busy ({reason}): refused a {weight}-unit {class} job ({used}/{cap} units)"
        ));
        state.metrics.inc(if batch {
            crate::metrics::Outcome::BusyHeadroom
        } else {
            crate::metrics::Outcome::Busy
        });
        let _ = tx
            .send(msg(server_msg::Msg::Busy(Busy {
                reason: reason.into(),
                units_used: used,
                capacity: cap,
            })))
            .await;
        return;
    };
    let (used, cap) = state.usage.snapshot();
    crate::log(format_args!(
        "accepted a {weight}-unit {class} job ({used}/{cap} units)"
    ));
    if tx
        .send(msg(server_msg::Msg::Accepted(Accepted {
            units: weight,
            units_used: used,
            capacity: cap,
            worker: cfg.name.clone(),
        })))
        .await
        .is_err()
    {
        return; // shim gone before we started: nothing to do
    }
    state.metrics.inc(crate::metrics::Outcome::Accepted);
    if batch {
        state.metrics.inc(crate::metrics::Outcome::BatchAccepted);
    }
    let job_id = state.metrics.new_job_id();
    let started = Instant::now();

    let (kill_tx, kill_rx) = watch::channel(false);
    let ctl = Arc::new(Ctl {
        fenced: AtomicBool::new(false),
        done: AtomicBool::new(false),
        last_heard: Mutex::new(Instant::now()),
        kill: kill_tx,
        paused: AtomicBool::new(false),
        quitting: AtomicBool::new(false),
        progress: std::sync::Mutex::new((Instant::now(), String::new(), false)),
        ended: std::sync::Mutex::new(None),
        state: state.clone(),
        job_id,
    });

    // A4: "preempted before start" -- the preempt handle exists (it was registered atomically
    // with the reservation, before Accepted was even sent) before this check, so a PLAYBACK
    // admission that preempted us between Accepted and here is caught here, before ffmpeg ever
    // spawns.
    if let Some(preempt) = guard.preempt.clone() {
        if preempt.is_preempted() {
            crate::log(format_args!("batch job preempted before it started"));
            ctl.done.store(true, Ordering::SeqCst);
            state.metrics.inc(crate::metrics::Outcome::Preempted);
            state
                .metrics
                .observe_seconds(started.elapsed().as_secs_f64());
            let _ = tx
                .send(msg(server_msg::Msg::Exit(Exit {
                    // Never spawned; nonzero (mirroring a mid-run SIGKILL's -9) so this can never
                    // be read as success by `code` alone -- the shim only inspects `preempted`.
                    code: -9,
                    fenced: false,
                    gpu_filters: false,
                    preempted: true,
                })))
                .await;
            drop(guard);
            return;
        }
    }

    let (stdin_tx, stdin_rx) = mpsc::channel::<Option<Vec<u8>>>(64);
    let stdin_rx = Arc::new(Mutex::new(stdin_rx));

    // Client -> us: stdin keys, heartbeats. Stream end or error = shim gone -> fence.
    let reader = {
        let ctl = ctl.clone();
        tokio::spawn(async move {
            loop {
                match inbound.message().await {
                    Ok(Some(m)) => {
                        *ctl.last_heard.lock().await = Instant::now();
                        match m.msg {
                            Some(client_msg::Msg::Stdin(b)) => {
                                // Jellyfin's throttler keys: p = pause, u = resume, q = quit
                                let key = b.iter().rev().find(|k| matches!(k, b'p' | b'u' | b'q'));
                                match key {
                                    Some(b'p') => ctl.paused.store(true, Ordering::SeqCst),
                                    Some(b'u') => ctl.paused.store(false, Ordering::SeqCst),
                                    Some(_) => ctl.quitting.store(true, Ordering::SeqCst),
                                    None => {}
                                }
                                let _ = stdin_tx.send(Some(b)).await;
                            }
                            Some(client_msg::Msg::StdinClose(_)) => {
                                let _ = stdin_tx.send(None).await;
                            }
                            _ => {}
                        }
                    }
                    _ => {
                        ctl.fence("shim closed the connection");
                        return;
                    }
                }
            }
        })
    };
    // Watchdog: never touches the stream, so a blocked send can't stall it.
    let watchdog = {
        let ctl = ctl.clone();
        let fence_after = cfg.fence_after;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                if ctl.last_heard.lock().await.elapsed() > fence_after {
                    ctl.fence(&format!(
                        "no frame from shim for {}s",
                        fence_after.as_secs_f64()
                    ));
                    return;
                }
            }
        })
    };
    let heartbeat = {
        let tx = tx.clone();
        let ctl = ctl.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if tx
                    .send(msg(server_msg::Msg::Heartbeat(Heartbeat {})))
                    .await
                    .is_err()
                {
                    ctl.fence("shim unreachable");
                    return;
                }
            }
        })
    };

    // Progress watchdog: ffmpeg's `time=` must advance unless Jellyfin paused it. A stalled
    // encode under a healthy agent would otherwise hang the session (it keeps heartbeating).
    let stall_watch = {
        let ctl = ctl.clone();
        // BATCH (trickplay) jobs get their own, much longer pair: see
        // `Config::batch_stall_after`'s doc comment for why the PLAYBACK-tuned defaults would
        // false-positive on a healthy, slowly-progressing trickplay job.
        let (stall, grace) = cfg.stall_limits(batch);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if ctl.done.load(Ordering::SeqCst) {
                    return;
                }
                let detail = {
                    let mut p = ctl.progress.lock().unwrap_or_else(|p| p.into_inner());
                    if ctl.paused.load(Ordering::SeqCst) || ctl.quitting.load(Ordering::SeqCst) {
                        p.0 = Instant::now(); // a pause is not a stall
                        None
                    } else {
                        let limit = if p.2 { stall } else { grace };
                        (p.0.elapsed() > limit)
                            .then(|| format!("no progress for {}s (time={})", limit.as_secs(), p.1))
                    }
                };
                if let Some(d) = detail {
                    ctl.end(Ended::Stalled, &d);
                    return;
                }
            }
        })
    };
    // Drain: on SIGTERM let the current segment finish, then end the job so Jellyfin restarts
    // it on another worker (the drill-proven restart path).
    let drain_watch = {
        let ctl = ctl.clone();
        let mut drain = state.drain.clone();
        let mapped: Vec<String> = job.args.iter().map(|a| map_path(a, &cfg.pathmap)).collect();
        tokio::spawn(async move {
            while !*drain.borrow() {
                if drain.changed().await.is_err() {
                    return;
                }
            }
            if batch {
                // A jpg sequence has no segment boundary to wait for (last_segment_index is
                // meaningless here, always None): end it right away so Jellyfin/the shim see a
                // clean, immediate exit rather than a 10s wait for something that never resolves.
                ctl.end(
                    Ended::Drained,
                    "agent draining (batch job has no segment boundary to wait for)",
                );
                return;
            }
            let base = tcpool_ir::last_segment_index(&mapped);
            let started = Instant::now();
            loop {
                if ctl.done.load(Ordering::SeqCst) {
                    return;
                }
                let now = tcpool_ir::last_segment_index(&mapped);
                if now > base || started.elapsed() > Duration::from_secs(10) {
                    ctl.end(Ended::Drained, &format!("agent draining (segment {now:?})"));
                    return;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
    };
    // Preemption watchdog (BATCH jobs only, `guard.preempt` is `None` for PLAYBACK): poll the
    // flag `reserve_playback` sets when it preempts this reservation, and end the job the same
    // way the stall/drain watchdogs do.
    let preempt_watch = guard.preempt.clone().map(|preempt| {
        let ctl = ctl.clone();
        tokio::spawn(async move {
            loop {
                if ctl.done.load(Ordering::SeqCst) {
                    return;
                }
                if preempt.is_preempted() {
                    ctl.end(Ended::Preempted, "preempted by a playback admission");
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    });

    let opts = TranslateOpts {
        pathmap: cfg.pathmap.clone(),
        gpu_filters: cfg.gpu_filters,
        rate_control: cfg.rate_control,
        source,
    };
    let cwd = map_path(&job.cwd, &cfg.pathmap);
    let render_fn = |o: &TranslateOpts| {
        let mut r = if batch {
            render_trickplay(&job.args, cfg.backend, o)
        } else {
            render(&job.args, cfg.backend, o)
        };
        if let Some((size, dur)) = cfg.probe_clamp {
            tcpool_ir::clamp_probe(&mut r.args, size, dur);
        }
        // P5: the DV7->8.1 marker is Jellyfin's request to this agent, never an ffmpeg option
        // worth keeping; whatever runs below (plain remux, fallback, GPU/CPU re-run) runs
        // without it.
        if wants_dv81(&r.args) {
            r.args = strip_dv81_signal(&r.args);
        }
        r
    };
    let mut rendered = render_fn(&opts);
    let mut code = if !batch && wants_dv81(&job.args) {
        run_dv81(
            cfg,
            &state,
            &rendered.args,
            &cwd,
            &tx,
            &ctl,
            kill_rx.clone(),
            stdin_rx.clone(),
        )
        .await
    } else {
        run_ffmpeg(
            cfg,
            &rendered.args,
            &cwd,
            &tx,
            &ctl,
            kill_rx.clone(),
            stdin_rx.clone(),
            None,
        )
        .await
    };
    if code != 0
        && rendered.gpu_filters
        && !ctl.fenced.load(Ordering::SeqCst)
        && ctl.ended() != Some(Ended::Drained)
        && !first_segment(&rendered.args).is_some_and(|p| std::path::Path::new(&p).exists())
    {
        // The GPU chain failed before producing anything (an unusual source, a filter this card
        // or driver rejects). Re-run with the CPU chain on this worker; Jellyfin is still waiting
        // for the first segment and never sees the failed attempt.
        crate::log(format_args!("GPU filters failed (exit {code}) before the first segment; re-running with CPU filters"));
        // Non-terminal: the job still ends in exit_ok/exit_error/fenced/... below once the
        // CPU-filter re-run finishes, so this counter is additional to that, not exclusive.
        state
            .metrics
            .inc(crate::metrics::Outcome::GpuFilterFallback);
        rendered = render_fn(&TranslateOpts {
            gpu_filters: false,
            ..opts.clone()
        });
        code = run_ffmpeg(
            cfg,
            &rendered.args,
            &cwd,
            &tx,
            &ctl,
            kill_rx,
            stdin_rx,
            None,
        )
        .await;
    }
    ctl.done.store(true, Ordering::SeqCst);
    let fenced = ctl.fenced.load(Ordering::SeqCst);
    let (outcome, preempted) = final_outcome(code, fenced, ctl.ended());
    state.metrics.inc(outcome);
    state
        .metrics
        .observe_seconds(started.elapsed().as_secs_f64());
    state.metrics.clear_speed(job_id);
    // A preempted BATCH job must always get an explicit Exit{preempted:true} so the shim can
    // apply A1's frame-exists split (fall back to a local re-run, or exit non-zero with no
    // rerun) -- unlike a plain fence, which the shim already treats as "worker gone" without
    // one. This only ever widens the send for a job that was preempted (a
    // PLAYBACK job's `ended` can never be `Preempted`: only a BATCH reservation gets a preempt
    // handle), so PLAYBACK behavior (`if !fenced`) is unchanged.
    if !fenced || preempted {
        let _ = tx
            .send(msg(server_msg::Msg::Exit(Exit {
                code,
                fenced,
                gpu_filters: rendered.gpu_filters,
                preempted,
            })))
            .await;
    }
    crate::log(format_args!(
        "exit {code}{}",
        if fenced { " (fenced)" } else { "" }
    ));
    heartbeat.abort();
    watchdog.abort();
    stall_watch.abort();
    drain_watch.abort();
    if let Some(h) = preempt_watch {
        h.abort();
    }
    reader.abort();
    drop(guard);
}

async fn pump(mut from: impl tokio::io::AsyncRead + Unpin, tx: Tx, ctl: Arc<Ctl>, stderr: bool) {
    let mut buf = vec![0u8; 8192];
    loop {
        match from.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if stderr {
                    ctl.note_stderr(&buf[..n]);
                }
                let m = if stderr {
                    server_msg::Msg::Stderr(buf[..n].to_vec())
                } else {
                    server_msg::Msg::Stdout(buf[..n].to_vec())
                };
                if tx.send(msg(m)).await.is_err() {
                    ctl.fence("shim unreachable");
                    return;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// P5: DV profile 7 -> 8.1 remux (transcode/docs/PLAN.md). ffmpeg#1 demuxes the video to MPEG-TS,
// this process rewrites it (dv81_ts + dv81), ffmpeg#2 muxes Jellyfin's HLS from it plus the
// source's other streams. ffmpeg#2 reads the video on fd 3 (`pipe:3`), NOT stdin: stdin stays
// Jellyfin's key channel (p/u/q throttling), exactly as on the plain path.
// ---------------------------------------------------------------------------------------------

/// The fd ffmpeg#2 reads the rewritten TS from.
const DV81_FD: i32 = 3;
/// How much of ffmpeg#1's TS output may pass without an in-band RPU before deciding the source's
/// RPU is out-of-band (the `hvcE` Block Addition muxing) and running the plain remux. Every DV
/// frame carries an RPU, so a real in-band source decides on its first frame; the limit only
/// bounds how long a no-RPU source delays its first segment.
const DV81_SCAN_BYTES: usize = 32 << 20;
/// Upper bound on the decision itself (ffmpeg#1 start + the scan), same reasoning. Kept well
/// inside the playback first-progress grace (`TC_FIRST_PROGRESS_GRACE`, 45 s default): the
/// grace timer starts before `source_dovi`'s ffprobe (up to 10 s) and this scan, and whatever
/// ffmpeg runs next (converting or the plain-remux fallback) still has to print its first
/// `time=` inside it, or the stall watchdog ends the job.
const DV81_SCAN_TIMEOUT: Duration = Duration::from_secs(10);

type Dv81Transform = fn(&[u8]) -> Result<crate::dv81_ts::TransformOut, String>;
type Dv81Rewriter = crate::dv81_ts::TsRewriter<Dv81Transform>;

fn dv81_transform(data: &[u8]) -> Result<crate::dv81_ts::TransformOut, String> {
    let (out, st) = crate::dv81::convert_access_unit(data)?;
    Ok((out, st.rpus, st.dropped_el))
}

/// ffmpeg#1, past the point where it has proven its output carries in-band RPUs.
struct Dv81Feed {
    demux: Child,
    stdout: tokio::process::ChildStdout,
    rewriter: Dv81Rewriter,
    /// Rewritten bytes produced during the scan, not yet written to ffmpeg#2.
    pending: Vec<u8>,
    /// ffmpeg#1's output already ended during the scan (a short clip).
    eof: bool,
}

enum Dv81Scan {
    Ready(Box<Dv81Feed>),
    Fallback(crate::metrics::Dv81Outcome, String),
    Killed,
}

/// Why this signaled job cannot convert, before anything is spawned; `Ok` = go.
async fn dv81_prepare(
    cfg: &Config,
    args: &[String],
) -> Result<
    (crate::dv81_plan::Plan, crate::dv81_ts::DoviDescriptor),
    (crate::metrics::Dv81Outcome, String),
> {
    use crate::metrics::Dv81Outcome as O;
    if !is_video_copy(args) {
        return Err((O::FallbackError, "not a video stream copy".into()));
    }
    let input = input_path(args).ok_or((O::FallbackError, "no input".to_string()))?;
    let Some(src) = crate::probe::source_dovi(cfg, input).await else {
        return Err((O::FallbackNotP7, "ffprobe failed or timed out".into()));
    };
    if src.profile != Some(7) {
        return Err((
            O::FallbackNotP7,
            format!("source DV profile is {:?}, not 7", src.profile),
        ));
    }
    let plan = crate::dv81_plan::plan(
        args,
        &crate::dv81_plan::Source {
            video_index: src.video_index,
            start_time: src.start_time,
        },
        DV81_FD,
    )
    .map_err(|e| (O::FallbackError, e))?;
    Ok((plan, crate::dv81_ts::DoviDescriptor::profile_81(src.level)))
}

/// Start ffmpeg#1 and read until its output proves (or disproves) in-band RPUs.
async fn dv81_scan(
    cfg: &Config,
    demux_args: &[String],
    cwd: &str,
    desc: crate::dv81_ts::DoviDescriptor,
    kill_rx: &mut watch::Receiver<bool>,
) -> Dv81Scan {
    use crate::metrics::Dv81Outcome as O;
    crate::log(format_args!(
        "dv81 demux: {} {}",
        cfg.ffmpeg,
        demux_args.join(" ")
    ));
    let mut cmd = Command::new(&cfg.ffmpeg);
    cmd.args(demux_args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if !cwd.is_empty() && std::path::Path::new(cwd).is_dir() {
        cmd.current_dir(cwd);
    }
    let mut demux = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Dv81Scan::Fallback(O::FallbackError, format!("demux spawn: {e}")),
    };
    let Some(mut stdout) = demux.stdout.take() else {
        return Dv81Scan::Fallback(O::FallbackError, "demux has no stdout".into());
    };
    // ffmpeg#1's stderr goes to the agent log, never to the shim (Jellyfin parses ffmpeg#2's).
    if let Some(mut err) = demux.stderr.take() {
        tokio::spawn(async move {
            let mut text = Vec::new();
            let _ = err.read_to_end(&mut text).await;
            let text = String::from_utf8_lossy(&text);
            let text = text.trim();
            if !text.is_empty() {
                let tail: String = text
                    .chars()
                    .rev()
                    .take(2000)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                crate::log(format_args!("dv81 demux stderr: {tail}"));
            }
        });
    }
    let mut rewriter: Dv81Rewriter = crate::dv81_ts::TsRewriter::new(desc, dv81_transform);
    let mut pending = Vec::new();
    let mut read_total = 0usize;
    let mut buf = vec![0u8; 256 << 10];
    let deadline = tokio::time::Instant::now() + DV81_SCAN_TIMEOUT;
    loop {
        if rewriter.stats.rpus > 0 {
            return Dv81Scan::Ready(Box::new(Dv81Feed {
                demux,
                stdout,
                rewriter,
                pending,
                eof: false,
            }));
        }
        if read_total >= DV81_SCAN_BYTES {
            return Dv81Scan::Fallback(
                O::FallbackNoRpu,
                format!(
                    "no in-band RPU (NAL 62) in the first {} MB ({} video frames)",
                    DV81_SCAN_BYTES >> 20,
                    rewriter.stats.video_pes
                ),
            );
        }
        let n = tokio::select! {
            r = tokio::time::timeout_at(deadline, stdout.read(&mut buf)) => r,
            changed = kill_rx.changed() => {
                if changed.is_err() || *kill_rx.borrow() {
                    return Dv81Scan::Killed;
                }
                continue;
            }
        };
        match n {
            Err(_) => {
                return Dv81Scan::Fallback(
                    O::FallbackError,
                    format!("no decision within {}s", DV81_SCAN_TIMEOUT.as_secs()),
                )
            }
            Ok(Err(e)) => return Dv81Scan::Fallback(O::FallbackError, format!("demux read: {e}")),
            Ok(Ok(0)) => {
                if let Err(e) = rewriter.finish(&mut pending) {
                    return Dv81Scan::Fallback(O::FallbackError, format!("rewrite: {e}"));
                }
                let status = demux.wait().await;
                if !status.as_ref().is_ok_and(|s| s.success()) {
                    return Dv81Scan::Fallback(
                        O::FallbackError,
                        format!("demux exited {}", exit_code(status)),
                    );
                }
                if rewriter.stats.rpus == 0 {
                    return Dv81Scan::Fallback(
                        O::FallbackNoRpu,
                        format!(
                            "no in-band RPU (NAL 62) in the whole output ({} video frames)",
                            rewriter.stats.video_pes
                        ),
                    );
                }
                return Dv81Scan::Ready(Box::new(Dv81Feed {
                    demux,
                    stdout,
                    rewriter,
                    pending,
                    eof: true,
                }));
            }
            Ok(Ok(n)) => {
                read_total += n;
                if let Err(e) = rewriter.push(&buf[..n], &mut pending) {
                    return Dv81Scan::Fallback(O::FallbackError, format!("rewrite: {e}"));
                }
            }
        }
    }
}

/// Pump ffmpeg#1 -> rewriter -> ffmpeg#2's fd 3 until ffmpeg#1 ends. `Err` = the conversion
/// itself failed (bad RPU, ffmpeg#1 died): the caller kills ffmpeg#2 so a truncated stream can
/// never look like a clean end. ffmpeg#2 going away (Jellyfin quit it) is `Ok`.
async fn dv81_feed(
    mut feed: Box<Dv81Feed>,
    mut out: tokio::net::unix::pipe::Sender,
) -> Result<crate::dv81_ts::TsStats, String> {
    if out.write_all(&feed.pending).await.is_err() {
        return Ok(feed.rewriter.stats);
    }
    feed.pending = Vec::new();
    let mut buf = vec![0u8; 256 << 10];
    let mut chunk = Vec::with_capacity(512 << 10);
    while !feed.eof {
        let n = feed
            .stdout
            .read(&mut buf)
            .await
            .map_err(|e| format!("demux read: {e}"))?;
        chunk.clear();
        if n == 0 {
            feed.rewriter.finish(&mut chunk)?;
            let status = feed.demux.wait().await;
            if !status.as_ref().is_ok_and(|s| s.success()) {
                return Err(format!("demux exited {}", exit_code(status)));
            }
            feed.eof = true;
        } else {
            feed.rewriter.push(&buf[..n], &mut chunk)?;
        }
        if out.write_all(&chunk).await.is_err() {
            break; // ffmpeg#2 closed its end: it is exiting on its own
        }
    }
    Ok(feed.rewriter.stats)
}

/// A signaled (DV7 -> 8.1) PLAYBACK job: convert if the gate passes, else run an HDR10-safe
/// fallback of `args` (Jellyfin's plain remux, marker stripped, `add_dv_removal_bsf` applied --
/// see that function's doc comment for why a fallback must not just copy whatever DV the source
/// actually has). Counts exactly one `tcpool_dv81_total` outcome.
#[allow(clippy::too_many_arguments)]
async fn run_dv81(
    cfg: &Config,
    state: &Arc<State>,
    args: &[String],
    cwd: &str,
    tx: &Tx,
    ctl: &Arc<Ctl>,
    mut kill_rx: watch::Receiver<bool>,
    stdin_rx: Arc<Mutex<mpsc::Receiver<Option<Vec<u8>>>>>,
) -> i32 {
    use crate::metrics::Dv81Outcome as O;
    // Defense in depth: `run_job`'s `render_fn` already strips the marker before calling here, so
    // this is normally a no-op. Stripping again (idempotent) means `run_dv81` never depends on its
    // caller having done it -- the fixture tests in `dv81_it.rs` call this directly with the
    // marker still present, and neither ffmpeg#1/#2's argv nor the fallback below should ever
    // carry it.
    let stripped = strip_dv81_signal(args);
    let args = &stripped;
    // Every fallback below runs this, never bare `args`: none of them produced a real 8.1 record,
    // so the client (which only ever asked for "8.1 or HDR10") must get DV removed rather than
    // whatever DV the source actually carries -- see `add_dv_removal_bsf`'s doc comment.
    let fallback_args = add_dv_removal_bsf(args);
    let fallback = |outcome: O, why: String| {
        crate::log(format_args!(
            "dv81: {}: {why}; running the plain remux with DV removed",
            outcome.as_str()
        ));
        state.metrics.inc_dv81(outcome);
    };
    let (plan, desc) = match dv81_prepare(cfg, args).await {
        Ok(p) => p,
        Err((o, why)) => {
            fallback(o, why);
            return run_ffmpeg(cfg, &fallback_args, cwd, tx, ctl, kill_rx, stdin_rx, None).await;
        }
    };
    let feed = match dv81_scan(cfg, &plan.demux, cwd, desc, &mut kill_rx).await {
        Dv81Scan::Ready(f) => f,
        Dv81Scan::Fallback(o, why) => {
            fallback(o, why);
            return run_ffmpeg(cfg, &fallback_args, cwd, tx, ctl, kill_rx, stdin_rx, None).await;
        }
        Dv81Scan::Killed => {
            // Fenced/drained/stalled while deciding: nothing ran for Jellyfin, nothing to fall
            // back to (the kill is job-wide). Still one outcome per signaled job.
            crate::log(format_args!("dv81: job killed during the RPU scan"));
            state.metrics.inc_dv81(O::FallbackError);
            return -libc::SIGKILL;
        }
    };
    crate::log(format_args!(
        "dv81: in-band RPU found; converting (level {})",
        desc.level
    ));
    let code = run_ffmpeg(
        cfg,
        &plan.mux,
        cwd,
        tx,
        ctl,
        kill_rx.clone(),
        stdin_rx.clone(),
        Some(feed),
    )
    .await;
    if code != 0
        && !ctl.fenced.load(Ordering::SeqCst)
        && ctl.ended().is_none()
        && !*kill_rx.borrow()
        && !first_segment(args).is_some_and(|p| std::path::Path::new(&p).exists())
    {
        fallback(
            O::FallbackError,
            format!("converting pipeline exited {code} before the first segment"),
        );
        return run_ffmpeg(cfg, &fallback_args, cwd, tx, ctl, kill_rx, stdin_rx, None).await;
    }
    state.metrics.inc_dv81(O::Converted);
    code
}

/// Run one ffmpeg to completion (or until fenced). Returns its exit code (-signal if killed).
/// With `feed` (P5), the rewritten DV8.1 TS is piped to the child's fd `DV81_FD`; a conversion
/// error kills the child.
#[allow(clippy::too_many_arguments)]
async fn run_ffmpeg(
    cfg: &Config,
    args: &[String],
    cwd: &str,
    tx: &Tx,
    ctl: &Arc<Ctl>,
    mut kill_rx: watch::Receiver<bool>,
    stdin_rx: Arc<Mutex<mpsc::Receiver<Option<Vec<u8>>>>>,
    feed: Option<Box<Dv81Feed>>,
) -> i32 {
    crate::log(format_args!("start: {} {}", cfg.ffmpeg, args.join(" ")));
    let mut cmd = Command::new(&cfg.ffmpeg);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if !cwd.is_empty() && std::path::Path::new(cwd).is_dir() {
        cmd.current_dir(cwd);
    }
    let mut pipe_tx = None;
    let mut pipe_rx_fd = None;
    if feed.is_some() {
        match tokio::net::unix::pipe::pipe().and_then(|(tx, rx)| Ok((tx, rx.into_blocking_fd()?))) {
            Ok((ptx, prx)) => {
                use std::os::fd::AsRawFd;
                let raw = prx.as_raw_fd();
                // SAFETY: runs in the forked child before exec; dup2/fcntl are async-signal-safe
                // and touch only this child's descriptor table.
                unsafe {
                    cmd.pre_exec(move || {
                        if raw == DV81_FD {
                            // Already fd 3: just let it survive exec.
                            if libc::fcntl(raw, libc::F_SETFD, 0) < 0 {
                                return Err(std::io::Error::last_os_error());
                            }
                        } else if libc::dup2(raw, DV81_FD) < 0 {
                            // dup2 clears FD_CLOEXEC on the new descriptor.
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
                pipe_tx = Some(ptx);
                pipe_rx_fd = Some(prx);
            }
            Err(e) => {
                crate::log(format_args!("dv81 pipe failed: {e}"));
                return 127;
            }
        }
    }
    let mut child: Child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            crate::log(format_args!("spawn failed: {e}"));
            return 127;
        }
    };
    drop(pipe_rx_fd); // the child has its copy; ours would only keep the pipe open
    let mut feeder = match (feed, pipe_tx) {
        (Some(f), Some(ptx)) => Some(tokio::spawn(dv81_feed(f, ptx))),
        _ => None,
    };
    if *kill_rx.borrow() {
        let _ = child.start_kill();
    }
    let out = child
        .stdout
        .take()
        .map(|o| tokio::spawn(pump(o, tx.clone(), ctl.clone(), false)));
    let err = child
        .stderr
        .take()
        .map(|e| tokio::spawn(pump(e, tx.clone(), ctl.clone(), true)));
    let writer = child.stdin.take().map(|mut stdin| {
        tokio::spawn(async move {
            let mut rx = stdin_rx.lock().await;
            while let Some(item) = rx.recv().await {
                match item {
                    Some(b) => {
                        if stdin.write_all(&b).await.is_err() || stdin.flush().await.is_err() {
                            return;
                        }
                    }
                    None => return, // stdin closed by Jellyfin: drop our end
                }
            }
        })
    });
    let status = loop {
        tokio::select! {
            s = child.wait() => break s,
            changed = kill_rx.changed() => {
                if changed.is_err() || *kill_rx.borrow() {
                    let _ = child.start_kill();
                    break child.wait().await;
                }
            }
            fed = async { feeder.as_mut().expect("guarded").await }, if feeder.is_some() => {
                feeder = None;
                match fed {
                    Ok(Ok(st)) => crate::log(format_args!(
                        "dv81: demux finished: {} frames, {} RPUs rewritten, {} EL NALs dropped",
                        st.video_pes, st.rpus, st.dropped_el
                    )),
                    Ok(Err(e)) => {
                        crate::log(format_args!("dv81: conversion failed mid-stream: {e}; killing ffmpeg"));
                        let _ = child.start_kill();
                        break child.wait().await;
                    }
                    Err(e) => {
                        crate::log(format_args!("dv81: feeder task died: {e}; killing ffmpeg"));
                        let _ = child.start_kill();
                        break child.wait().await;
                    }
                }
            }
        }
    };
    if let Some(f) = feeder {
        f.abort(); // drops ffmpeg#1 (kill_on_drop)
    }
    if let Some(w) = writer {
        w.abort();
    }
    for h in [out, err].into_iter().flatten() {
        let _ = tokio::time::timeout(Duration::from_secs(2), h).await;
    }
    exit_code(status)
}

fn exit_code(status: std::io::Result<std::process::ExitStatus>) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    match status {
        Ok(s) => s.code().unwrap_or_else(|| -s.signal().unwrap_or(1)),
        Err(_) => 1,
    }
}

#[cfg(test)]
#[path = "dv81_it.rs"]
mod dv81_it;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_refusal_only_applies_to_batch_jobs() {
        let mut cfg = Config::minimal();
        cfg.accept_batch = false;
        assert_eq!(
            batch_refusal_reason(&cfg, false),
            None,
            "a playback job is never refused for BATCH reasons"
        );
    }

    #[test]
    fn batch_refused_when_the_worker_opted_out() {
        let mut cfg = Config::minimal();
        cfg.accept_batch = false;
        cfg.policy.trickplay_output_root = Some("/scratch/tp".into());
        assert_eq!(batch_refusal_reason(&cfg, true), Some("batch-disabled"));
    }

    #[test]
    fn batch_refused_when_no_trickplay_output_root_is_configured() {
        let mut cfg = Config::minimal();
        cfg.policy.trickplay_output_root = None;
        assert_eq!(batch_refusal_reason(&cfg, true), Some("batch-disabled"));
    }

    #[test]
    fn batch_allowed_when_accepted_and_rooted() {
        let mut cfg = Config::minimal();
        cfg.policy.trickplay_output_root = Some("/scratch/tp".into());
        assert_eq!(batch_refusal_reason(&cfg, true), None);
    }

    #[test]
    fn final_outcome_a_clean_exit_wins_over_a_late_preempted_ended() {
        // Regression: preempt_watch (or stall_watch/drain_watch) can record an `Ended` reason in
        // the narrow window between ffmpeg actually exiting 0 and `ctl.done` being stored (the
        // pump-drain await inside run_ffmpeg). A code-0, not-fenced exit must always win.
        assert_eq!(
            final_outcome(0, false, Some(Ended::Preempted)),
            (crate::metrics::Outcome::ExitOk, false)
        );
        assert_eq!(
            final_outcome(0, false, Some(Ended::Stalled)),
            (crate::metrics::Outcome::ExitOk, false)
        );
        assert_eq!(
            final_outcome(0, false, Some(Ended::Drained)),
            (crate::metrics::Outcome::ExitOk, false)
        );
        assert_eq!(
            final_outcome(0, false, None),
            (crate::metrics::Outcome::ExitOk, false)
        );
    }

    #[test]
    fn final_outcome_reports_a_genuine_preemption() {
        assert_eq!(
            final_outcome(-9, false, Some(Ended::Preempted)),
            (crate::metrics::Outcome::Preempted, true)
        );
    }

    #[test]
    fn final_outcome_fenced_wins_regardless_of_code_or_ended() {
        assert_eq!(
            final_outcome(0, true, Some(Ended::Preempted)),
            (crate::metrics::Outcome::Fenced, false)
        );
        assert_eq!(
            final_outcome(-9, true, None),
            (crate::metrics::Outcome::Fenced, false)
        );
    }

    #[test]
    fn final_outcome_nonzero_no_ended_is_exit_error() {
        assert_eq!(
            final_outcome(1, false, None),
            (crate::metrics::Outcome::ExitError, false)
        );
    }
}
