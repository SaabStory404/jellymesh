//! tcpool-shim: installed as Jellyfin's `ffmpeg`. HLS transcodes run on the pool; everything
//! else (ffprobe-style calls, trickplay, `-version`) execs the real ffmpeg unchanged.
//!
//! Contract with Jellyfin (MEASURED from 12.x source + the spike's drills):
//! - While the first segment does not exist, Jellyfin waits with no timeout, so any failure
//!   before it is retried on another worker, invisibly. Exiting non-zero then would wedge the
//!   session (Jellyfin 12 never unregisters the job).
//! - After the first segment, a lost worker -> exit 255: Jellyfin restarts ffmpeg at the next
//!   missing segment and the new shim picks a surviving worker.
//! - Never panic into a non-zero exit before the first segment: panics fall back to local ffmpeg.
//!
//! Worker discovery, re-done on every `discover()` call so the pool tracks the DaemonSet:
//! - `TC_WORKERS_DNS=host[:port]` (a headless Service, e.g.
//!   `tcpool-agents.media.svc.cluster.local.:9901`) — every A/AAAA record is one agent;
//! - `TC_WORKERS=name=host:port,...` — the static fallback, still honoured, and merged in for any
//!   address DNS did not return.

mod lease;
#[cfg(test)]
mod tests;

use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tcpool_ir::{first_segment, is_hls_transcode, playlist, required_output};
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

/// Spread load: most free capacity (as a fraction) first, then GPU before CPU, then discovery
/// order.
fn rank(mut ws: Vec<Worker>, need: Option<&str>) -> Vec<Worker> {
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
    let free = |w: &Worker| {
        if w.caps.capacity > 0.0 {
            (w.caps.capacity - w.caps.units_used) / w.caps.capacity
        } else {
            0.0
        }
    };
    ws.sort_by(|a, b| {
        free(b)
            .partial_cmp(&free(a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(kind_rank(&a.caps.kind).cmp(&kind_rank(&b.caps.kind)))
            .then(a.order.cmp(&b.order))
    });
    ws
}

enum Attempt {
    Busy,
    Unreachable(String),
    Exited(i32),
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

async fn attempt(w: &Worker, args: &[String], sink: &StdinSink) -> Attempt {
    let (tx, rx) = mpsc::channel::<ClientMsg>(64);
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let job = Job {
        args: args.to_vec(),
        cwd,
        priority: Priority::Playback as i32,
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
    match tokio::time::timeout(ADMIT_TIMEOUT, inbound.message()).await {
        Ok(Ok(Some(m))) => match m.msg {
            Some(server_msg::Msg::Accepted(a)) => {
                log(&format!(
                    "transcode -> worker {} ({} units, {:.1}/{:.1})",
                    w.label(),
                    a.units,
                    a.units_used,
                    a.capacity
                ));
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
        Err(_) => return Attempt::Unreachable(format!("no admission within {ADMIT_TIMEOUT:?}")),
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
                Some(server_msg::Msg::Exit(e)) => break Attempt::Exited(e.code),
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
            // the new shim acquires the lease.
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
    let mut tried: Vec<String> = Vec::new();
    loop {
        let candidates: Vec<Worker> = rank(discover(tls.clone()).await, need)
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
            match attempt(w, &args, &sink).await {
                Attempt::Busy => continue,
                Attempt::Unreachable(e) => {
                    log(&format!("worker {} unreachable at run: {e}", w.label()));
                    tried.push(w.name.clone());
                    continue;
                }
                Attempt::Exited(0) => {
                    log(&format!("worker {} finished with exit code 0", w.label()));
                    return finish(Outcome::Exit(0));
                }
                r => {
                    let first_done =
                        first_segment(&args).is_some_and(|f| std::path::Path::new(&f).exists());
                    let why = match &r {
                        Attempt::Exited(c) => format!("exit {c}"),
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
                            log(&format!("worker {} LOST mid-transcode ({why}); exiting 255 so Jellyfin restarts", w.label()));
                            return finish(Outcome::Exit(255));
                        }
                        Attempt::Exited(c) => {
                            // the agent ended it (stall watchdog, drain) or ffmpeg failed
                            // mid-stream: Jellyfin restarts at the next missing segment
                            remove_partials(&args);
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

fn main() {
    let raw: Vec<OsString> = std::env::args_os().skip(1).collect();
    let args: Vec<String> = raw
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    if !is_hls_transcode(&args) {
        exec_real(&raw);
    }
    log(&format!("transcode request: {}", args.join(" ")));
    let outcome = std::panic::catch_unwind(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map(|rt| rt.block_on(run(args)))
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
