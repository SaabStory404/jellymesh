//! tcpool-agent: one per transcode device. Probes what the card can do, admits jobs by
//! resolution-weighted capacity, and runs jellyfin-ffmpeg adapted to the card.

mod config;
mod job;
mod metrics;
mod probe;

use config::Config;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tcpool_proto::worker_server::{Worker, WorkerServer};
use tcpool_proto::{client_msg, Caps, ClientMsg, HelloRequest, ServerMsg};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;
use tonic::{Request, Response, Status, Streaming};

pub fn log(args: std::fmt::Arguments<'_>) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() % 86400;
    println!(
        "{:02}:{:02}:{:02}.{:03} {}",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60,
        now.subsec_millis(),
        args
    );
}

/// Units in use; admission is atomic here, so concurrent shims can never overbook a card.
pub struct Usage {
    inner: Mutex<(f64, u32)>,
    capacity: f64,
}

pub struct Reservation<'a> {
    usage: &'a Usage,
    units: f64,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let mut g = self.usage.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.0 = (g.0 - self.units).max(0.0);
        g.1 = g.1.saturating_sub(1);
    }
}

impl Usage {
    pub fn reserve(&self, units: f64) -> Option<Reservation<'_>> {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if g.0 + units > self.capacity + 1e-9 {
            return None;
        }
        g.0 += units;
        g.1 += 1;
        Some(Reservation { usage: self, units })
    }

    pub fn snapshot(&self) -> (f64, f64) {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        (g.0, self.capacity)
    }

    fn jobs(&self) -> u32 {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).1
    }
}

#[cfg(test)]
mod usage_tests {
    use super::*;
    use proptest::prelude::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    fn make_usage(capacity: f64) -> Usage {
        Usage {
            inner: Mutex::new((0.0, 0)),
            capacity,
        }
    }

    #[test]
    fn reserve_admits_up_to_capacity_and_refuses_over() {
        let u = make_usage(2.0);
        let a = u.reserve(1.5).expect("fits");
        assert!(
            u.reserve(1.0).is_none(),
            "0.5 unit of headroom, 1.0 requested"
        );
        let b = u.reserve(0.5).expect("fits exactly");
        assert_eq!(u.snapshot(), (2.0, 2.0));
        drop(a);
        drop(b);
        assert_eq!(u.snapshot(), (0.0, 2.0));
        assert_eq!(u.jobs(), 0);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        // Property (3): weighted admission never overbooks and always drains back to 0, under
        // concurrent reserve/drop from multiple threads with random unit sizes.
        #[test]
        fn concurrent_reserve_drop_never_overbooks(
            capacity in 1.0f64..20.0,
            units in prop::collection::vec(0.05f64..6.0, 30..150),
        ) {
            let usage = Arc::new(make_usage(capacity));
            let n_threads = 6usize;
            let chunk_size = (units.len() / n_threads).max(1);
            let chunks: Vec<Vec<f64>> = units.chunks(chunk_size).map(|c| c.to_vec()).collect();
            let overbooked = Arc::new(AtomicBool::new(false));

            let handles: Vec<_> = chunks
                .into_iter()
                .map(|chunk| {
                    let usage = Arc::clone(&usage);
                    let overbooked = Arc::clone(&overbooked);
                    thread::spawn(move || {
                        for u in chunk {
                            if let Some(r) = usage.reserve(u) {
                                let (used, cap) = usage.snapshot();
                                if used > cap + 1e-6 {
                                    overbooked.store(true, Ordering::SeqCst);
                                }
                                // Interleave a little before releasing.
                                thread::yield_now();
                                drop(r);
                            }
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }

            prop_assert!(
                !overbooked.load(Ordering::SeqCst),
                "units_used exceeded capacity during concurrent admission"
            );
            let (used, _cap) = usage.snapshot();
            prop_assert!(used.abs() < 1e-6, "units_used did not return to 0: {used}");
            prop_assert_eq!(usage.jobs(), 0);
        }
    }
}

pub struct State {
    pub cfg: Config,
    pub probed: probe::Probed,
    pub usage: Usage,
    pub probed_unix: i64,
    /// Set on SIGTERM: stop admitting; running jobs end after their current segment.
    pub drain: tokio::sync::watch::Receiver<bool>,
    pub metrics: metrics::Metrics,
}

#[derive(Clone)]
struct Svc(Arc<State>);

type RunStream = Pin<Box<dyn Stream<Item = Result<ServerMsg, Status>> + Send>>;

#[tonic::async_trait]
impl Worker for Svc {
    async fn hello(&self, _: Request<HelloRequest>) -> Result<Response<Caps>, Status> {
        let s = &self.0;
        let (used, cap) = s.usage.snapshot();
        Ok(Response::new(Caps {
            name: s.cfg.name.clone(),
            kind: s.cfg.backend.as_str().into(),
            outputs: s.probed.outputs.clone(),
            gpu_tonemap: s.probed.gpu_tonemap,
            capacity: cap,
            units_used: used,
            active_jobs: s.usage.jobs(),
            ffmpeg_version: s.probed.ffmpeg_version.clone(),
            agent_version: env!("CARGO_PKG_VERSION").into(),
            probed_unix: s.probed_unix,
            node: s.cfg.node.clone(),
        }))
    }

    type RunStream = RunStream;

    async fn run(&self, req: Request<Streaming<ClientMsg>>) -> Result<Response<RunStream>, Status> {
        let mut inbound = req.into_inner();
        let first = tokio::time::timeout(std::time::Duration::from_secs(10), inbound.message())
            .await
            .map_err(|_| Status::deadline_exceeded("no job"))??;
        let job = match first.and_then(|m| m.msg) {
            Some(client_msg::Msg::Job(j)) => j,
            _ => return Err(Status::invalid_argument("first message must be a job")),
        };
        // The security boundary: only Jellyfin's HLS transcode shape may run here.
        let mapped: Vec<String> = job
            .args
            .iter()
            .map(|a| tcpool_ir::map_path(a, &self.0.cfg.pathmap))
            .collect();
        if let Err(e) = tcpool_ir::validate::validate(&mapped, &self.0.cfg.policy) {
            log(format_args!("REFUSED a job: {e}"));
            self.0.metrics.inc(metrics::Outcome::RefusedPolicy);
            return Err(Status::permission_denied(e));
        }
        let (tx, rx) = mpsc::channel(128);
        tokio::spawn(job::run_job(self.0.clone(), job, inbound, tx));
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

#[tokio::main]
async fn main() {
    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tcpool-agent: {e}");
            std::process::exit(2);
        }
    };
    // CUDA JIT cache: the worker's HOME does not exist in the pod, so without this every ffmpeg
    // recompiled its CUDA kernels (~12 s on the P4, MEASURED). Warmed by the startup probe.
    if std::env::var_os("CUDA_CACHE_PATH").is_none() {
        std::env::set_var("CUDA_CACHE_PATH", "/tmp/cuda-cache");
    }
    // Probe before serving: not ready until the card is characterised.
    let probed = probe::probe(&cfg).await;
    let probed_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], cfg.port));
    log(format_args!(
        "[{}/{}] listening on {addr} ffmpeg={} {} outputs={:?} gpu_tonemap={} capacity={} (4K={} 1440={} copy={})",
        cfg.name, cfg.backend, cfg.ffmpeg, probed.ffmpeg_version, probed.outputs, probed.gpu_tonemap,
        cfg.capacity, cfg.weight_4k, cfg.weight_1440, cfg.weight_copy
    ));
    let capacity = cfg.capacity;
    let (drain_tx, drain_rx) = tokio::sync::watch::channel(false);
    let state = Arc::new(State {
        cfg,
        probed,
        usage: Usage {
            inner: Mutex::new((0.0, 0)),
            capacity,
        },
        probed_unix,
        drain: drain_rx,
        metrics: metrics::Metrics::default(),
    });

    if let Some(port) = state.cfg.metrics_port {
        let m = state.clone();
        tokio::spawn(async move { metrics::serve(m, port).await });
    }

    let (health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter.set_serving::<WorkerServer<Svc>>().await;
    health_reporter
        .set_service_status("", tonic_health::ServingStatus::Serving)
        .await;

    let tls = match tcpool_proto::tls::TlsFiles::from_env() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("tcpool-agent: {e}");
            std::process::exit(2);
        }
    };
    // kubelet's gRPC probes cannot speak TLS: health (only) on a separate plaintext port.
    // Health is always served on the main port. With TLS, kubelet's gRPC probe can't reach it
    // there, so it is ALSO served in plaintext on TC_HEALTH_PORT (default 9902 when TLS is on).
    // Without TLS no second port is opened unless asked for: several local agents on one host
    // would otherwise collide on a fixed default (MEASURED in the protocol suite).
    let health_port = std::env::var("TC_HEALTH_PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .or(if tls.is_some() { Some(9902) } else { None });
    if let Some(health_port) = health_port {
        let hs = health_service.clone();
        tokio::spawn(async move {
            let addr = std::net::SocketAddr::from(([0, 0, 0, 0], health_port));
            if let Err(e) = tonic::transport::Server::builder()
                .add_service(hs)
                .serve(addr)
                .await
            {
                eprintln!("tcpool-agent: health server on :{health_port}: {e}");
                std::process::exit(1);
            }
        });
    }
    // cert-manager rotates the certificate in place: drain (like SIGTERM) and let the container
    // restart with the new one, so rotation never cuts a session.
    let (rotate_tx, mut rotate_rx) = tokio::sync::watch::channel(false);
    if let Some(files) = tls.clone() {
        let first = files.modified();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                if files.modified() != first {
                    log(format_args!(
                        "TLS certificate changed on disk; draining to restart with it"
                    ));
                    let _ = rotate_tx.send(true);
                    return;
                }
            }
        });
    }

    // Graceful drain: stop admitting and report not-ready at once, let each running job finish
    // its current segment (job.rs ends it there, Jellyfin restarts it elsewhere), then exit.
    // Bounded well inside terminationGracePeriodSeconds.
    let drain_state = state.clone();
    let shutdown = async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("sigterm");
        tokio::select! {
            _ = term.recv() => {}
            _ = rotate_rx.changed() => {}
        }
        log(format_args!(
            "SIGTERM: draining {} job(s)",
            drain_state.usage.jobs()
        ));
        let _ = drain_tx.send(true);
        health_reporter
            .set_service_status("", tonic_health::ServingStatus::NotServing)
            .await;
        health_reporter.set_not_serving::<WorkerServer<Svc>>().await;
        let started = std::time::Instant::now();
        while drain_state.usage.jobs() > 0 && started.elapsed() < std::time::Duration::from_secs(12)
        {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        // let the final exit messages reach the shims
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        log(format_args!(
            "drained in {:.1}s; stopping",
            started.elapsed().as_secs_f64()
        ));
        // Every job has ended; don't let HTTP/2 graceful shutdown of idle client connections
        // stretch past terminationGracePeriodSeconds (MEASURED +8 s without this).
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            std::process::exit(0);
        });
    };
    let mut builder = tonic::transport::Server::builder();
    if let Some(files) = &tls {
        let cfg = match files.server_config() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("tcpool-agent: tls: {e}");
                std::process::exit(2);
            }
        };
        builder = match builder.tls_config(cfg) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("tcpool-agent: tls: {e}");
                std::process::exit(2);
            }
        };
        log(format_args!(
            "mTLS on: clients must present a certificate from the pool CA"
        ));
    } else {
        log(format_args!(
            "WARNING plaintext gRPC (no TC_TLS_*): lab/dev only"
        ));
    }
    if let Err(e) = builder
        .http2_keepalive_interval(Some(std::time::Duration::from_secs(2)))
        .http2_keepalive_timeout(Some(std::time::Duration::from_secs(4)))
        .add_service(health_service)
        .add_service(WorkerServer::new(Svc(state)))
        .serve_with_shutdown(addr, shutdown)
        .await
    {
        eprintln!("tcpool-agent: server: {e}");
        std::process::exit(1);
    }
}
