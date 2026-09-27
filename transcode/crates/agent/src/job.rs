//! One job: admission, ffmpeg lifecycle, heartbeats, fencing, and the CPU-filter re-run.

use crate::config::Config;
use crate::probe::source_height;
use crate::State;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tcpool_ir::{first_segment, input_path, is_video_copy, map_path, render, TranslateOpts};
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

/// Units this job costs on this card.
async fn job_weight(cfg: &Config, args: &[String]) -> f64 {
    if is_video_copy(args) {
        return cfg.weight_copy;
    }
    let input = input_path(args).map(|p| map_path(p, &cfg.pathmap));
    let height = match input {
        Some(p) => source_height(cfg, &p).await,
        None => None,
    };
    cfg.weight_for_height(height)
}

/// Why the agent ended a job itself (not fencing).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ended {
    Stalled,
    Drained,
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

pub async fn run_job(state: Arc<State>, job: Job, mut inbound: Streaming<ClientMsg>, tx: Tx) {
    let cfg = &state.cfg;
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
    let weight = job_weight(cfg, &job.args).await;
    let Some(guard) = state.usage.reserve(weight) else {
        let (used, cap) = state.usage.snapshot();
        crate::log(format_args!(
            "busy: refused a {weight}-unit job ({used}/{cap} units)"
        ));
        state.metrics.inc(crate::metrics::Outcome::Busy);
        let _ = tx
            .send(msg(server_msg::Msg::Busy(Busy {
                reason: "capacity".into(),
                units_used: used,
                capacity: cap,
            })))
            .await;
        return;
    };
    let (used, cap) = state.usage.snapshot();
    crate::log(format_args!(
        "accepted a {weight}-unit job ({used}/{cap} units)"
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
        let (stall, grace) = (cfg.stall_after, cfg.first_progress_grace);
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

    let opts = TranslateOpts {
        pathmap: cfg.pathmap.clone(),
        gpu_filters: cfg.gpu_filters,
    };
    let cwd = map_path(&job.cwd, &cfg.pathmap);
    let render = |o: &TranslateOpts| {
        let mut r = render(&job.args, cfg.backend, o);
        if let Some((size, dur)) = cfg.probe_clamp {
            tcpool_ir::clamp_probe(&mut r.args, size, dur);
        }
        r
    };
    let mut rendered = render(&opts);
    let mut code = run_ffmpeg(
        cfg,
        &rendered.args,
        &cwd,
        &tx,
        &ctl,
        kill_rx.clone(),
        stdin_rx.clone(),
    )
    .await;
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
        rendered = render(&TranslateOpts {
            gpu_filters: false,
            ..opts.clone()
        });
        code = run_ffmpeg(cfg, &rendered.args, &cwd, &tx, &ctl, kill_rx, stdin_rx).await;
    }
    ctl.done.store(true, Ordering::SeqCst);
    let fenced = ctl.fenced.load(Ordering::SeqCst);
    let outcome = if fenced {
        crate::metrics::Outcome::Fenced
    } else {
        match ctl.ended() {
            Some(Ended::Stalled) => crate::metrics::Outcome::Stalled,
            // Drained jobs are killed to end them (a nonzero/signal exit code), but that's a
            // planned handoff to another worker, not a failure -- keep it out of exit_error.
            Some(Ended::Drained) => crate::metrics::Outcome::Drained,
            None if code == 0 => crate::metrics::Outcome::ExitOk,
            None => crate::metrics::Outcome::ExitError,
        }
    };
    state.metrics.inc(outcome);
    state
        .metrics
        .observe_seconds(started.elapsed().as_secs_f64());
    state.metrics.clear_speed(job_id);
    if !fenced {
        let _ = tx
            .send(msg(server_msg::Msg::Exit(Exit {
                code,
                fenced,
                gpu_filters: rendered.gpu_filters,
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

/// Run one ffmpeg to completion (or until fenced). Returns its exit code (-signal if killed).
async fn run_ffmpeg(
    cfg: &Config,
    args: &[String],
    cwd: &str,
    tx: &Tx,
    ctl: &Arc<Ctl>,
    mut kill_rx: watch::Receiver<bool>,
    stdin_rx: Arc<Mutex<mpsc::Receiver<Option<Vec<u8>>>>>,
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
    let mut child: Child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            crate::log(format_args!("spawn failed: {e}"));
            return 127;
        }
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
        }
    };
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
