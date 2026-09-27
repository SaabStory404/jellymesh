//! Agent configuration from the environment (same variable names as the spike).

use std::time::Duration;
use tcpool_ir::Backend;

#[derive(Debug, Clone)]
pub struct Config {
    pub name: String,
    pub node: String,
    pub backend: Backend,
    pub ffmpeg: String,
    pub ffprobe: String,
    pub port: u16,
    /// Prometheus text endpoint port; off unless `TC_METRICS_PORT` is set (k8s sets 9903).
    pub metrics_port: Option<u16>,
    pub pathmap: Vec<(String, String)>,
    /// Resolution-weighted capacity in units.
    pub capacity: f64,
    pub weight_1440: f64,
    pub weight_4k: f64,
    /// Units a stream copy costs (I/O only, no decode/encode).
    pub weight_copy: f64,
    /// Operator restriction on the probed outputs (never an addition).
    pub outputs_allow: Option<Vec<String>>,
    pub gpu_filters: bool,
    /// Upper bounds for Jellyfin's `-probesize`/`-analyzeduration` (bytes, microseconds); None = off.
    pub probe_clamp: Option<(u64, u64)>,
    /// Kill ffmpeg after this long without a client message (must stay below the shim's 6 s).
    pub fence_after: Duration,
    /// End a job whose ffmpeg reports no progress for this long while not paused.
    pub stall_after: Duration,
    /// ... and before its first progress line (probing a large remux over NFS takes a while).
    pub first_progress_grace: Duration,
    /// Command allowlist roots (worker-side paths).
    pub policy: tcpool_ir::validate::Policy,
}

fn env(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

impl Config {
    pub fn from_env() -> Result<Config, String> {
        let kind = env("TC_KIND", "cpu");
        let backend =
            Backend::parse(&kind).ok_or_else(|| format!("TC_KIND={kind}: want qsv|nvenc|cpu"))?;
        let ffmpeg = env("TC_FFMPEG", "/usr/lib/jellyfin-ffmpeg/ffmpeg");
        let ffprobe = match std::path::Path::new(&ffmpeg).parent() {
            Some(dir) if !dir.as_os_str().is_empty() => {
                dir.join("ffprobe").to_string_lossy().into_owned()
            }
            _ => "ffprobe".into(),
        };
        let pathmap = env("TC_PATHMAP", "")
            .split(',')
            .filter_map(|p| {
                p.split_once('=')
                    .map(|(a, b)| (a.to_string(), b.to_string()))
            })
            .collect();
        // TC_CAPACITY wins; TC_MAX_JOBS is the flat-count fallback (weight 1 per job).
        let max_jobs = env_f64("TC_MAX_JOBS", 0.0);
        let capacity = match env_f64("TC_CAPACITY", 0.0) {
            c if c > 0.0 => c,
            _ if max_jobs > 0.0 => max_jobs,
            _ => 1000.0,
        };
        let weighted = env_f64("TC_CAPACITY", 0.0) > 0.0;
        Ok(Config {
            name: env("TC_NAME", &kind),
            node: env("NODE_NAME", ""),
            backend,
            ffmpeg,
            ffprobe,
            port: env("TC_PORT", "9901")
                .parse()
                .map_err(|e| format!("TC_PORT: {e}"))?,
            metrics_port: std::env::var("TC_METRICS_PORT")
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| {
                    v.parse::<u16>()
                        .map_err(|e| format!("TC_METRICS_PORT: {e}"))
                })
                .transpose()?,
            pathmap,
            capacity,
            weight_1440: if weighted {
                env_f64("TC_WEIGHT_1440", 2.0)
            } else {
                1.0
            },
            weight_4k: if weighted {
                env_f64("TC_WEIGHT_4K", 3.0)
            } else {
                1.0
            },
            weight_copy: if weighted {
                env_f64("TC_WEIGHT_COPY", 0.25)
            } else {
                1.0
            },
            outputs_allow: std::env::var("TC_OUTPUTS")
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| v.split(',').map(str::to_string).collect()),
            gpu_filters: env("TC_HW_FILTERS", "1") != "0",
            // TC_PROBE_CLAMP="probesize,analyzeduration" (ffmpeg size syntax), "0" = off. See
            // tcpool_ir::clamp_probe for why the default is 50M,5M.
            probe_clamp: {
                let v = env("TC_PROBE_CLAMP", "50M,5M");
                v.split_once(',')
                    .and_then(|(p, a)| Some((tcpool_ir::parse_size(p)?, tcpool_ir::parse_size(a)?)))
            },
            fence_after: Duration::from_secs_f64(env_f64("TC_FENCE_AFTER", 3.0)),
            stall_after: Duration::from_secs_f64(env_f64("TC_STALL_AFTER", 20.0)),
            first_progress_grace: Duration::from_secs_f64(env_f64("TC_FIRST_PROGRESS_GRACE", 45.0)),
            policy: {
                let d = tcpool_ir::validate::Policy::default();
                let list = |name: &str, dflt: Vec<String>| match std::env::var(name) {
                    Ok(v) if !v.is_empty() => v.split(',').map(str::to_string).collect(),
                    _ => dflt,
                };
                tcpool_ir::validate::Policy {
                    input_roots: list("TC_INPUT_ROOTS", d.input_roots),
                    read_roots: list("TC_READ_ROOTS", d.read_roots),
                    output_root: env("TC_OUTPUT_ROOT", &d.output_root),
                }
            },
        })
    }

    /// Units for a source of `height` pixels (`None` = unknown -> the most expensive weight).
    pub fn weight_for_height(&self, height: Option<u32>) -> f64 {
        match height {
            Some(h) if h <= 1100 => 1.0, // <= 1080p, incl. 1920x1080 with bars
            Some(h) if h <= 1700 => self.weight_1440,
            _ => self.weight_4k,
        }
    }
}
