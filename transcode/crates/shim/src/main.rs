//! tcpool-shim: installed as Jellyfin's `ffmpeg`. HLS transcodes run on the pool at
//! `Priority::Playback`; a trickplay (sprite extraction) command additionally routes to the pool
//! at `Priority::Batch` when `TC_BATCH=1` and its output directory is under
//! `TC_TRICKPLAY_OUTPUT_ROOT` (`batch_enabled`) -- off by default, so this ships dark. Everything
//! else (ffprobe-style calls, chapter images, `-version`, and trickplay when the gate is off)
//! execs the real ffmpeg unchanged.
//!
//! Contract with Jellyfin (MEASURED from 12.x source + the spike's drills), for PLAYBACK:
//! - While the first segment does not exist, Jellyfin waits with no timeout, so any failure
//!   before it is retried on another worker, invisibly. Exiting non-zero then would wedge the
//!   session (Jellyfin 12 never unregisters the job).
//! - After the first segment, a lost worker -> exit 255: Jellyfin restarts ffmpeg at the next
//!   missing segment and the new shim picks a surviving worker.
//! - Never panic into a non-zero exit before the first segment: panics fall back to local ffmpeg.
//!
//! BATCH (`run_batch`) is a separate, much simpler control path: one admission round (no
//! lease/`follow()`, no retry-across-workers loop), bounded by `ADMIT_TIMEOUT_BATCH`. A clean
//! pool finish exits 0. Busy/unreachable candidates, and the round's own deadline, fall back to
//! a local `exec_real` (nothing has run yet). A pool *failure* mid-job (preempted, lost, or a
//! non-zero exit) splits on whether a first frame already landed on disk (A1, REVISED): no frame
//! yet -> `exec_real` locally, same as any other fallback; a frame exists -> the shim exits
//! non-zero itself, WITHOUT rerunning locally, since a from-scratch local rerun cannot catch up
//! to the pool's high-water mark inside Jellyfin's ~20s trickplay poll window (see `run_batch`'s
//! doc comment).
//!
//! Worker discovery, re-done on every `discover()` call so the pool tracks the DaemonSet:
//! - `TC_WORKERS_DNS=host[:port]` (a headless Service, e.g.
//!   `tcpool-agents.media.svc.cluster.local.:9901`) — every A/AAAA record is one agent;
//! - `TC_WORKERS=name=host:port,...` — the static fallback, still honoured, and merged in for any
//!   address DNS did not return.
//!
//! Seek affinity (PLAYBACK only, `TC_AFFINITY`, default on): once a session picks a worker, later
//! restarts of the same HLS output (seek, track change, bitrate switch) prefer that same worker
//! instead of ranking fresh, so an ordinary seek can't visibly hop the session between the Arc
//! (QSV) and the P4 (NVENC). See the `affinity` module for the key, the store and the failover
//! contract.

mod affinity;
mod lease;
#[cfg(test)]
mod tests;

use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tcpool_ir::{first_segment, playlist, required_output};
use tcpool_proto::worker_client::WorkerClient;
use tcpool_proto::{
    client_msg, server_msg, Caps, ClientMsg, Heartbeat, HelloRequest, Job, Priority,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

const DEAD_AFTER: Duration = Duration::from_secs(6);
const HELLO_TIMEOUT: Duration = Duration::from_millis(1500);
/// Accepted/Busy arrives after the agent's ffprobe of the source (bounded at 10 s there).
const ADMIT_TIMEOUT: Duration = Duration::from_secs(15);

fn real_ffmpeg() -> String {
    std::env::var("TC_FFMPEG_REAL")
        .unwrap_or_else(|_| "/usr/lib/jellyfin-ffmpeg/ffmpeg.real".into())
}

fn log(msg: &str) {
    let path = std::env::var("TC_SHIM_LOG").unwrap_or_else(|_| "/config/log/tc-shim.log".into());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let s = now.as_secs() % 86400;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(
            f,
            "{:02}:{:02}:{:02}.{:03} [{}] {msg}",
            s / 3600,
            (s / 60) % 60,
            s % 60,
            now.subsec_millis(),
            std::process::id()
        );
    }
}

fn exec_real(raw: &[OsString]) -> ! {
    let real = real_ffmpeg();
    let err = std::process::Command::new(&real).args(raw).exec();
    eprintln!("tcpool-shim: exec {real}: {err}");
    std::process::exit(127);
}

struct Worker {
    name: String,
    channel: Channel,
    caps: Caps,
    order: usize,
}

impl Worker {
    /// Name for log lines: the agent's own `TC_NAME` from `Hello` when it reported one, else the
    /// address we dialled. `self.name` stays the identity used for the already-tried list, because
    /// two DaemonSet pods of the same class report the same `TC_NAME`.
    fn label(&self) -> &str {
        if self.caps.name.is_empty() {
            &self.name
        } else {
            &self.caps.name
        }
    }
}

/// One dialable agent: `name` is the identity (dedupe, already-tried list), `addr` is `host:port`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    name: String,
    addr: String,
}

/// How many times a `TC_WORKERS_DNS` lookup is retried before the round gives up. A headless
/// Service with no Ready endpoint is NXDOMAIN, which getaddrinfo reports exactly like a CoreDNS
/// blip, so the two cannot be told apart here: retry, then fall through to an empty pool.
const DNS_ATTEMPTS: usize = 3;
const DNS_RETRY: Duration = Duration::from_millis(300);

fn configured_workers() -> Vec<(String, String)> {
    std::env::var("TC_WORKERS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|item| {
            let (name, addr) = item.trim().split_once('=')?;
            Some((name.trim().to_string(), addr.trim().to_string()))
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
fn merge_targets(dns: Vec<Target>, statics: Vec<(String, String)>) -> Vec<Target> {
    let mut out = dns;
    for (name, addr) in statics {
        if !out.iter().any(|t| t.addr == addr) {
            out.push(Target { name, addr });
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

/// Hello every discovered worker at once; keep the ones that answer.
async fn discover(tls: Option<ClientTlsConfig>) -> Vec<Worker> {
    let mut tasks = Vec::new();
    for (order, Target { name, addr }) in targets().await.into_iter().enumerate() {
        let tls = tls.clone();
        tasks.push(tokio::spawn(async move {
            let mut ep =
                Endpoint::from_shared(tcpool_proto::tls::endpoint_url(&addr, tls.is_some()))
                    .ok()?;
            if let Some(t) = tls {
                ep = match ep.tls_config(t) {
                    Ok(e) => e,
                    Err(e) => {
                        log(&format!("worker {name}: tls config: {e}"));
                        return None;
                    }
                };
            }
            let ep = ep
                .connect_timeout(HELLO_TIMEOUT)
                .http2_keep_alive_interval(Duration::from_secs(2))
                .keep_alive_timeout(Duration::from_secs(4));
            let res = tokio::time::timeout(HELLO_TIMEOUT, async {
                let channel = ep.connect().await?;
                let caps = WorkerClient::new(channel.clone())
                    .hello(HelloRequest {})
                    .await?
                    .into_inner();
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>((channel, caps))
            })
            .await;
            match res {
                Ok(Ok((channel, caps))) => Some(Worker {
                    name,
                    channel,
                    caps,
                    order,
                }),
                Ok(Err(e)) => {
                    log(&format!("worker {name} {addr} unreachable: {e}"));
                    None
                }
                Err(_) => {
                    log(&format!(
                        "worker {name} {addr} unreachable: no hello within {HELLO_TIMEOUT:?}"
                    ));
                    None
                }
            }
        }));
    }
    let mut out = Vec::new();
    for t in tasks {
        if let Ok(Some(w)) = t.await {
            out.push(w);
        }
    }
    out
}

/// Tie-break preference among equally free workers: a GPU before the CPU spill. With DNS
/// discovery `order` is whatever the resolver returned, so without this an idle pool (every worker
/// at free = 1.0) could send a 4K HDR job to the CPU worker.
fn kind_rank(kind: &str) -> u8 {
    match kind {
        "qsv" | "nvenc" => 0,
        "cpu" => 2,
        _ => 1,
    }
}

/// Fraction of `w`'s capacity currently unused (0 for an unset/zero `capacity`, which never ranks
/// ahead of anything with room). `discount_batch` (true for PLAYBACK ranking, false for BATCH's
/// own -- see `rank`'s doc comment) subtracts a worker's currently-running-but-preemptible BATCH
/// load from its `units_used` first: a worker mid-trickplay-job is not as "busy" to a PLAYBACK
/// candidate as its raw `units_used` suggests, since `reserve_playback` can preempt that load on
/// demand. BATCH candidates keep ranking on raw `units_used` unchanged -- discounting there would
/// make an already-batch-loaded worker look artificially free to more batch load.
fn free_fraction(w: &Worker, discount_batch: bool) -> f64 {
    if w.caps.capacity <= 0.0 {
        return 0.0;
    }
    let used = if discount_batch {
        (w.caps.units_used - w.caps.batch_units_used).max(0.0)
    } else {
        w.caps.units_used
    };
    (w.caps.capacity - used) / w.caps.capacity
}

/// Spread load: most free capacity (as a fraction) first, then GPU before CPU, then discovery
/// order.
fn rank(mut ws: Vec<Worker>, need: Option<&str>, discount_batch: bool) -> Vec<Worker> {
    ws.retain(|w| match need {
        Some(n) if !w.caps.outputs.iter().any(|o| o == n) => {
            log(&format!(
                "worker {} cannot output {n} (has {:?}); skipping",
                w.label(),
                w.caps.outputs
            ));
            false
        }
        _ => true,
    });
    ws.sort_by(|a, b| {
        free_fraction(b, discount_batch)
            .partial_cmp(&free_fraction(a, discount_batch))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(kind_rank(&a.caps.kind).cmp(&kind_rank(&b.caps.kind)))
            .then(a.order.cmp(&b.order))
    });
    ws
}

/// Move the worker pinned by a fresh, non-garbage affinity file to the front of an already-ranked
/// `PLAYBACK` list (never called for BATCH). Preferred only when it answered `Hello` this round
/// (so it survived `rank`'s `need` filter and is present in `ws`) and currently shows any free
/// capacity: the shim never learns a job's real weight before `Accepted`, so this is a coarse
/// "has room" check, same bar `run_batch` picks blind on. A false positive here just costs one
/// `Busy` reply -- `run`'s loop already falls through on `Attempt::Busy` to the next candidate in
/// the list this function produces, which is exactly how a *draining* affine worker gets skipped
/// too: `Caps` carries no draining flag for `rank` to check up front (the agent only reports
/// `Busy{reason:"draining"}` at admission time), so "not draining" is enforced by that same
/// existing fallback rather than a new field here.
fn apply_affinity(mut ws: Vec<Worker>, affine: &str) -> Vec<Worker> {
    match ws.iter().position(|w| w.name == affine) {
        Some(idx) if free_fraction(&ws[idx], true) > 0.0 => {
            let w = ws.remove(idx);
            log(&format!(
                "affinity hit: pinning worker {} to the front for this output",
                w.label()
            ));
            ws.insert(0, w);
        }
        Some(idx) => {
            log(&format!(
                "affinity miss (full): worker {} has no free capacity; ranking fresh",
                ws[idx].label()
            ));
        }
        None => {
            log(&format!(
                "affinity miss (absent): worker {affine} did not answer Hello this round or cannot take this output; ranking fresh"
            ));
        }
    }
    ws
}

enum Attempt {
    Busy,
    Unreachable(String),
    /// `(exit code, Exit.preempted)`. `preempted` is only ever set for a BATCH job (see
    /// `run_batch`); a PLAYBACK job's agent-side job task never has a preempt handle to trigger.
    Exited(i32, bool),
    Lost(String),
}

type StdinSink = Arc<Mutex<Option<mpsc::Sender<ClientMsg>>>>;

fn start_stdin_pump(sink: StdinSink) {
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            let n = stdin.read(&mut buf).unwrap_or(0);
            let m = if n == 0 {
                client_msg::Msg::StdinClose(true)
            } else {
                client_msg::Msg::Stdin(buf[..n].to_vec())
            };
            if let Some(tx) = sink.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
                let _ = tx.blocking_send(ClientMsg { msg: Some(m) });
            }
            if n == 0 {
                return;
            }
        }
    });
}

async fn attempt(
    w: &Worker,
    args: &[String],
    sink: &StdinSink,
    priority: Priority,
    admit_timeout: Duration,
    affinity_path: Option<&Path>,
) -> Attempt {
    let (tx, rx) = mpsc::channel::<ClientMsg>(64);
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let job = Job {
        args: args.to_vec(),
        cwd,
        priority: priority as i32,
    };
    if tx
        .send(ClientMsg {
            msg: Some(client_msg::Msg::Job(job)),
        })
        .await
        .is_err()
    {
        return Attempt::Unreachable("channel".into());
    }
    let mut client = WorkerClient::new(w.channel.clone());
    let mut inbound = match client.run(ReceiverStream::new(rx)).await {
        Ok(r) => r.into_inner(),
        Err(e) => return Attempt::Unreachable(e.to_string()),
    };
    match tokio::time::timeout(admit_timeout, inbound.message()).await {
        Ok(Ok(Some(m))) => match m.msg {
            Some(server_msg::Msg::Accepted(a)) => {
                log(&format!(
                    "transcode -> worker {} ({} units, {:.1}/{:.1})",
                    w.label(),
                    a.units,
                    a.units_used,
                    a.capacity
                ));
                // PLAYBACK only (BATCH callers pass None): pin this output to this worker's
                // identity so a later seek/track-change restart of the same output prefers it.
                if let Some(path) = affinity_path {
                    affinity::write(path, &w.name);
                }
            }
            Some(server_msg::Msg::Busy(b)) => {
                log(&format!(
                    "worker {} busy ({:.1}/{:.1} units); skipping",
                    w.label(),
                    b.units_used,
                    b.capacity
                ));
                return Attempt::Busy;
            }
            _ => return Attempt::Unreachable("protocol: expected accepted/busy".into()),
        },
        Ok(Ok(None)) => return Attempt::Unreachable("stream closed before admission".into()),
        Ok(Err(e)) => return Attempt::Unreachable(e.to_string()),
        Err(_) => return Attempt::Unreachable(format!("no admission within {admit_timeout:?}")),
    }
    // Running: forward stdin to this worker, heartbeat, relay output.
    *sink.lock().unwrap_or_else(|p| p.into_inner()) = Some(tx.clone());
    let hb = {
        let tx = tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if tx
                    .send(ClientMsg {
                        msg: Some(client_msg::Msg::Heartbeat(Heartbeat {})),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        })
    };
    let result = loop {
        match tokio::time::timeout(DEAD_AFTER, inbound.message()).await {
            Err(_) => break Attempt::Lost(format!("no frame for {}s", DEAD_AFTER.as_secs())),
            Ok(Err(e)) => break Attempt::Lost(format!("stream error: {e}")),
            Ok(Ok(None)) => break Attempt::Lost("connection closed".into()),
            Ok(Ok(Some(m))) => match m.msg {
                Some(server_msg::Msg::Stderr(b)) => {
                    let _ = std::io::stderr().write_all(&b);
                }
                Some(server_msg::Msg::Stdout(b)) => {
                    let _ = std::io::stdout().write_all(&b);
                }
                Some(server_msg::Msg::Exit(e)) => break Attempt::Exited(e.code, e.preempted),
                _ => {}
            },
        }
    };
    hb.abort();
    *sink.lock().unwrap_or_else(|p| p.into_inner()) = None;
    result
}

/// Remove half-written `<prefix>N.ts.tmp` files a lost worker left behind (the final names are
/// only ever complete segments, thanks to `-hls_flags temp_file`).
fn remove_partials(args: &[String]) {
    let Some((dir, prefix, _)) = tcpool_ir::segment_pattern(args) else {
        return;
    };
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with(&prefix) && n.ends_with(".tmp") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Exit(i32),
    Local,
}

/// Follow another replica's encode of this output; take over when its lease dies.
async fn follow(args: &[String], pl: &str) -> Option<Outcome> {
    let path = lease::lease_path(pl);
    log(&format!(
        "output {pl} is being encoded by another replica; following"
    ));
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if lease::is_fresh(&path) {
            continue;
        }
        let first_done = first_segment(args).is_some_and(|f| std::path::Path::new(&f).exists());
        if first_done {
            // Holder gone mid-stream: exit like a lost worker, Jellyfin restarts at the edge and
            // the new shim acquires the lease. The holder's own worker is presumed gone with it,
            // so clear any pin on this output before exiting, same as the direct-loss path below.
            if affinity::enabled() {
                affinity::clear(&affinity::affinity_path(pl));
                log("affinity cleared: lease holder gone");
            }
            log("lease holder gone after the first segment; exiting 255 so Jellyfin restarts");
            return Some(Outcome::Exit(255));
        }
        return None; // holder gone before producing anything: encode it ourselves
    }
}

async fn run(args: Vec<String>) -> Outcome {
    let sink: StdinSink = Arc::new(Mutex::new(None));
    start_stdin_pump(sink.clone());
    let pl = playlist(&args).unwrap_or_default().to_string();
    let lease = loop {
        match lease::try_acquire(&pl) {
            lease::Acquire::Held(l) => break Some(l),
            lease::Acquire::Unavailable(e) => {
                log(&format!("lease unavailable ({e}); running unleased"));
                break None;
            }
            lease::Acquire::Busy => {
                if let Some(o) = follow(&args, &pl).await {
                    return o;
                }
            }
        }
    };
    let finish = |o: Outcome| {
        if let Some(l) = &lease {
            l.release();
        }
        o
    };
    let need = required_output(&args);
    let tls = match tcpool_proto::tls::TlsFiles::from_env()
        .and_then(|f| f.map(|f| f.client_config()).transpose())
    {
        Ok(t) => t,
        Err(e) => {
            log(&format!(
                "WARNING tls misconfigured ({e}); cannot reach the pool, running LOCALLY"
            ));
            return finish(Outcome::Local);
        }
    };
    // Seek affinity (PLAYBACK only): the file next to `pl` that a prior round of *this same*
    // output pinned a worker into, on Accepted. Re-read every round (not just once) so a change
    // written by this same shim after an earlier round's Accepted is honoured immediately, and so
    // TC_AFFINITY_TTL_SECS expiring mid-retry falls back to fresh ranking without a restart.
    let affinity_path = affinity::affinity_path(&pl);
    // Opportunistic cleanup: every PLAYBACK output shares one scratch directory, so this also
    // reaps *other* sessions' orphaned `.worker` files, not just this one's own (see
    // `affinity::sweep_stale`'s doc comment). Throttled internally; cheap to call every round.
    if affinity::enabled() {
        // A malformed/adversarial command line that still classifies as Hls without a usable
        // playlist path (`playlist()` falls back to "") would otherwise resolve `.parent()` to
        // `Some("")` and have this write into the shim's CWD, not the scratch dir -- guard it.
        if let Some(dir) = affinity_path.parent().filter(|d| !d.as_os_str().is_empty()) {
            affinity::sweep_stale(dir, affinity::ttl_from_env());
        }
    }
    let mut tried: Vec<String> = Vec::new();
    loop {
        let ranked = rank(discover(tls.clone()).await, need, true);
        let ranked = if affinity::enabled() {
            match affinity::read(&affinity_path, affinity::ttl_from_env()) {
                Some(affine) => apply_affinity(ranked, &affine),
                None => ranked,
            }
        } else {
            ranked
        };
        let candidates: Vec<Worker> = ranked
            .into_iter()
            .filter(|w| !tried.contains(&w.name))
            .collect();
        if candidates.is_empty() {
            // Loud: a local software encode of a 4K HDR source cannot keep up (JellyMesh lab).
            log(&format!(
                "WARNING no worker can take this job (tried {tried:?}); running LOCALLY on CPU"
            ));
            return finish(Outcome::Local);
        }
        let mut progressed = false;
        for w in &candidates {
            match attempt(
                w,
                &args,
                &sink,
                priority_for(tcpool_ir::Shape::Hls),
                ADMIT_TIMEOUT,
                affinity::enabled().then_some(affinity_path.as_path()),
            )
            .await
            {
                Attempt::Busy => continue,
                Attempt::Unreachable(e) => {
                    log(&format!("worker {} unreachable at run: {e}", w.label()));
                    tried.push(w.name.clone());
                    continue;
                }
                Attempt::Exited(0, _) => {
                    log(&format!("worker {} finished with exit code 0", w.label()));
                    return finish(Outcome::Exit(0));
                }
                r => {
                    let first_done =
                        first_segment(&args).is_some_and(|f| std::path::Path::new(&f).exists());
                    let why = match &r {
                        Attempt::Exited(c, _) => format!("exit {c}"),
                        Attempt::Lost(e) => e.clone(),
                        _ => unreachable!(),
                    };
                    if !first_done {
                        log(&format!("worker {} failed before the first segment ({why}); re-running on another worker", w.label()));
                        tried.push(w.name.clone());
                        progressed = true;
                        break;
                    }
                    match r {
                        Attempt::Lost(_) => {
                            remove_partials(&args);
                            // Unclean loss: clear the pin so a restart ranks fresh instead of
                            // returning to a card that just died. A clean Busy/refusal (the
                            // `Attempt::Busy` arm above) never reaches here and never clears it.
                            if affinity::enabled() {
                                affinity::clear(&affinity_path);
                                log(&format!("affinity cleared: worker {} lost", w.label()));
                            }
                            log(&format!("worker {} LOST mid-transcode ({why}); exiting 255 so Jellyfin restarts", w.label()));
                            return finish(Outcome::Exit(255));
                        }
                        Attempt::Exited(c, _) => {
                            // the agent ended it (stall watchdog, drain) or ffmpeg failed
                            // mid-stream: Jellyfin restarts at the next missing segment. Same
                            // unclean-loss reasoning as the `Lost` arm above applies here too --
                            // a watchdog-killed worker has already released its units (so
                            // `apply_affinity` would rank it right back to the front) and this is
                            // exactly the wedge class the pin exists to fail over from, so clear
                            // it on any non-zero post-first-segment exit, not just a dropped
                            // connection.
                            remove_partials(&args);
                            if affinity::enabled() {
                                affinity::clear(&affinity_path);
                                log(&format!(
                                    "affinity cleared: worker {} exited {c} after the first segment",
                                    w.label()
                                ));
                            }
                            log(&format!("worker {} finished with exit code {c}", w.label()));
                            return finish(Outcome::Exit(c));
                        }
                        _ => unreachable!(),
                    }
                }
            }
        }
        if !progressed {
            // Everyone was busy or unreachable this round; a short wait before re-ranking keeps
            // Jellyfin's first-segment wait (no timeout) from spinning on a full pool.
            if candidates.iter().all(|w| tried.contains(&w.name)) {
                continue;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

/// The whole BATCH candidate round (discovery + admission across however many candidates it
/// takes) is bounded by this -- not per-candidate like PLAYBACK's `ADMIT_TIMEOUT`, and it does
/// NOT wrap a candidate once it's `Accepted`: only `DEAD_AFTER` applies from there, so a long
/// trickplay job is never killed by this clock. A3 wants the total round comfortably under
/// Jellyfin's ~20s trickplay responsiveness window.
const ADMIT_TIMEOUT_BATCH: Duration = Duration::from_secs(6);

/// A2: the shim routes a trickplay command to the pool only when `TC_BATCH=1` AND the output
/// directory is under `TC_TRICKPLAY_OUTPUT_ROOT`. Off by default (dark ship).
fn batch_enabled(args: &[String]) -> bool {
    if std::env::var("TC_BATCH").ok().as_deref() != Some("1") {
        return false;
    }
    let Some(root) = std::env::var("TC_TRICKPLAY_OUTPUT_ROOT")
        .ok()
        .filter(|v| !v.is_empty())
    else {
        return false;
    };
    let Some(dir) = tcpool_ir::trickplay_output_dir(args) else {
        return false;
    };
    under_trickplay_root(&dir, &root)
}

/// Canonical, symlink-safe check that `dir` is under `root`, once any *existing* ancestor's
/// symlinks are resolved. `dir` itself may not exist yet -- Jellyfin creates its trickplay temp
/// dir right before running ffmpeg -- so this walks up to the deepest existing ancestor,
/// canonicalizes only that, and rejoins the remaining (not-yet-created) components lexically:
/// they cannot smuggle a symlink escape since nothing on disk answers for them yet. A lexical
/// `..` anywhere in `dir` is rejected outright, regardless of whether it would cancel out --
/// that would depend on exactly what exists on disk at call time, which is not a property this
/// check can rely on. This is a defense-in-depth layer in front of the shim; the agent's own
/// allowlist (`Policy::trickplay_output_root`) independently enforces the same root.
fn under_trickplay_root(dir: &str, root: &str) -> bool {
    let dir_path = std::path::Path::new(dir);
    if !dir_path.is_absolute()
        || dir_path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return false;
    }
    let Ok(root_canon) = std::fs::canonicalize(root) else {
        return false; // root not configured/mounted on this host: fail closed
    };
    let mut existing = dir_path;
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if let Ok(canon) = std::fs::canonicalize(existing) {
            let mut full = canon;
            for seg in tail.iter().rev() {
                full.push(seg);
            }
            return full.starts_with(&root_canon);
        }
        let Some(name) = existing.file_name() else {
            return false;
        };
        tail.push(name);
        match existing.parent() {
            Some(p) if !p.as_os_str().is_empty() => existing = p,
            _ => return false,
        }
    }
}

/// The shape-to-priority mapping BATCH routing is built on (pure, so it's directly testable
/// without any network): `Shape::Trickplay` is the only shape ever routed at `Priority::Batch`.
fn priority_for(shape: tcpool_ir::Shape) -> Priority {
    match shape {
        tcpool_ir::Shape::Trickplay => Priority::Batch,
        _ => Priority::Playback,
    }
}

/// A1 (REVISED): the exit code the shim uses when a BATCH pool failure happened AFTER a first
/// frame was already written -- a case where a local rerun is refused (see `batch_pool_failure`).
/// Chosen to collide with nothing else the shim ever returns: not 0 (success), not 127
/// (`exec_real`'s own spawn-failure fallback), not 255 (PLAYBACK's lost-mid-transcode exit), and
/// not a value `code & 0xff` can produce for a killed process today (SIGKILL's -9 -> 247). Not a
/// signal-derived number at all, so a future kill signal added to the agent can't collide with it
/// either. No suite assertion greps for this literal.
const BATCH_PARTIAL_FAILURE_EXIT: i32 = 200;

/// Count of `.jpg`/`.jpeg` frame files already on disk for a trickplay job -- diagnostic only,
/// for the shim log line; never used to make the local-rerun decision (that's
/// `trickplay_first_frame` + a single `exists()` check in `batch_pool_failure`).
fn trickplay_frame_count(args: &[String]) -> usize {
    let Some(dir) = tcpool_ir::trickplay_output_dir(args) else {
        return 0;
    };
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| {
                    let n = e.file_name().to_string_lossy().to_lowercase();
                    n.ends_with(".jpg") || n.ends_with(".jpeg")
                })
                .count()
        })
        .unwrap_or(0)
}

/// A1 (REVISED): what a BATCH pool *failure* (preempted, lost, or a non-zero exit -- never a
/// clean 0) becomes, once split on whether the pool ever produced a first frame. `reason` is a
/// human description of the failure for the log line only.
///
/// - No first frame on disk -> the pool made no visible progress, so Jellyfin's
///   `jpegCount > lastCount` poll is satisfied identically whether the eventual count comes from
///   the pool or a from-scratch local run: `Outcome::Local` (`exec_real`) is always safe here.
/// - A first frame exists -> a from-scratch local rerun cannot pass the pool's high-water mark
///   inside Jellyfin's ~20s trickplay responsiveness window, so rerunning would either race a
///   half-finished pool output or silently produce a shorter (and wrong) sprite sheet. Instead
///   the shim exits `BATCH_PARTIAL_FAILURE_EXIT` and does NOT touch the output. Jellyfin throws
///   `FfmpegException`, deletes the temp dir and retries on the next scheduled trickplay task or
///   library scan -- the "retryable failure" PLAN §3.3 describes. Resuming from the high-water
///   mark instead of retrying whole is a documented P4 follow-up (docs/PLAN.md §10), not
///   implemented here.
fn batch_pool_failure(args: &[String], reason: &str) -> Outcome {
    let has_frame =
        tcpool_ir::trickplay_first_frame(args).is_some_and(|f| std::path::Path::new(&f).exists());
    if has_frame {
        let frames = trickplay_frame_count(args);
        log(&format!(
            "batch job {reason}; {frames} frame(s) already on disk; exiting {BATCH_PARTIAL_FAILURE_EXIT} without a local rerun"
        ));
        Outcome::Exit(BATCH_PARTIAL_FAILURE_EXIT)
    } else {
        log(&format!(
            "batch job {reason}; no frame on disk yet; running LOCALLY"
        ));
        Outcome::Local
    }
}

/// The BATCH control path (A1/A2/A3): a single admission round, no lease/`follow()` (there is no
/// `playlist()`-derived key to lease on, and no cross-replica dedup for BATCH in this pass).
/// `Outcome::Exit(0)` on a clean finish; every candidate busy or unreachable, or the admission
/// round's own deadline passing, is `Outcome::Local` (nothing has run yet, so a local run is
/// always safe); a pool *failure* (lost, agent-reported preemption, or any other non-zero exit)
/// goes through `batch_pool_failure`, which is `Outcome::Local` unless a first frame already
/// landed (A1 REVISED), in which case it is a non-zero `Outcome::Exit` with no local rerun.
async fn run_batch(args: Vec<String>) -> Outcome {
    let sink: StdinSink = Arc::new(Mutex::new(None));
    start_stdin_pump(sink.clone());
    let tls = match tcpool_proto::tls::TlsFiles::from_env()
        .and_then(|f| f.map(|f| f.client_config()).transpose())
    {
        Ok(t) => t,
        Err(e) => {
            log(&format!(
                "WARNING tls misconfigured ({e}); cannot reach the pool, running batch job LOCALLY"
            ));
            return Outcome::Local;
        }
    };
    let deadline = tokio::time::Instant::now() + ADMIT_TIMEOUT_BATCH;
    // mjpeg has no capability token to rank on (need = None); BATCH still prefers free
    // capacity/GPU-first via the same rank() PLAYBACK uses.
    // BATCH's own ranking keeps raw units_used (discount_batch=false): see rank()'s doc comment.
    let candidates = rank(discover(tls).await, None, false);
    if candidates.is_empty() {
        log("no worker reachable for a batch (trickplay) job; running LOCALLY");
        return Outcome::Local;
    }
    for w in &candidates {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            log(&format!(
                "batch admission round exceeded {ADMIT_TIMEOUT_BATCH:?}; running LOCALLY"
            ));
            return Outcome::Local;
        }
        match attempt(
            w,
            &args,
            &sink,
            priority_for(tcpool_ir::Shape::Trickplay),
            remaining,
            None, // BATCH never writes seek affinity
        )
        .await
        {
            Attempt::Busy => {
                log(&format!(
                    "worker {} busy for the batch job; trying next",
                    w.label()
                ));
                continue;
            }
            Attempt::Unreachable(e) => {
                log(&format!(
                    "worker {} unreachable for the batch job: {e}",
                    w.label()
                ));
                continue;
            }
            // Preempted (before or after producing output) takes priority over the exit code:
            // A1 treats it as a pool failure like any other, split by `batch_pool_failure` on
            // whether a first frame exists.
            Attempt::Exited(c, true) => {
                return batch_pool_failure(
                    &args,
                    &format!("on worker {} was preempted (exit {c})", w.label()),
                );
            }
            Attempt::Exited(0, false) => {
                log(&format!(
                    "worker {} finished the batch job with exit code 0",
                    w.label()
                ));
                return Outcome::Exit(0);
            }
            Attempt::Exited(c, false) => {
                return batch_pool_failure(&args, &format!("on worker {} exited {c}", w.label()));
            }
            Attempt::Lost(e) => {
                return batch_pool_failure(
                    &args,
                    &format!("on worker {} was lost ({e})", w.label()),
                );
            }
        }
    }
    log("every worker was busy or unreachable for the batch job; running LOCALLY");
    Outcome::Local
}

fn main() {
    let raw: Vec<OsString> = std::env::args_os().skip(1).collect();
    let args: Vec<String> = raw
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let shape = tcpool_ir::classify(&args);
    let batch = matches!(shape, tcpool_ir::Shape::Trickplay) && batch_enabled(&args);
    if !matches!(shape, tcpool_ir::Shape::Hls) && !batch {
        exec_real(&raw);
    }
    log(&format!("transcode request: {}", args.join(" ")));
    let outcome = std::panic::catch_unwind(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map(|rt| {
                if batch {
                    rt.block_on(run_batch(args))
                } else {
                    rt.block_on(run(args))
                }
            })
    });
    match outcome {
        Ok(Ok(Outcome::Exit(code))) => {
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            // exit (not return): the stdin pump thread may be blocked on Jellyfin's stdin
            std::process::exit(code & 0xff);
        }
        Ok(Ok(Outcome::Local)) => exec_real(&raw),
        Ok(Err(e)) => {
            log(&format!("runtime error ({e}); running locally"));
            exec_real(&raw)
        }
        Err(_) => {
            log("shim panicked; running locally");
            exec_real(&raw)
        }
    }
}
