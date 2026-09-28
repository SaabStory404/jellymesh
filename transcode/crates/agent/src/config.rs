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
    /// `stall_after`'s BATCH (trickplay) counterpart. `time=` only advances once per muxed
    /// frame, and trickplay's `fps=0.1`-style filter chain mints one frame per ~10 source-seconds
    /// -- on a single-threaded CPU-spill decode of a 4K/HDR source (an intended case: A5 lets a
    /// worker opt IN to BATCH), 10 source-seconds can easily take longer than the PLAYBACK-tuned
    /// default (20s) to decode, stalling the watchdog on a perfectly healthy job. Default is
    /// minutes, not tens of seconds; see `stall_limits`.
    pub batch_stall_after: Duration,
    /// `first_progress_grace`'s BATCH counterpart: ffmpeg prints `time=N/A` (dropped by
    /// `note_stderr`) until the first muxed frame, so the *first* trickplay frame is exposed to
    /// this grace, not `stall_after` -- it needs the same widening for the same reason.
    pub batch_first_progress_grace: Duration,
    /// Command allowlist roots (worker-side paths).
    pub policy: tcpool_ir::validate::Policy,
    /// Fixed admission weight for a BATCH (trickplay) job: A3, no ffprobe before admission.
    pub batch_weight: f64,
    /// TC_ACCEPT_BATCH: lets a worker (e.g. the CPU spill) opt out of BATCH entirely.
    pub accept_batch: bool,
    /// Units of capacity reserved exclusively for PLAYBACK bursts; BATCH admission never eats
    /// into this even when it's currently unused.
    pub batch_headroom: f64,
    /// Shared transcode dir (docs/SHARED-TRANSCODE.md): keep a PLAYBACK job whose shim went away
    /// running while its keepalive is fresh (`TC_DETACH`, default on; only jobs whose shim sent a
    /// `keepalive_path` are eligible, so old shims keep fence-on-loss).
    pub detach: bool,
    /// A detached job ends after this long without a keepalive touch (`TC_ORPHAN_IDLE_SECS`).
    pub orphan_idle: Duration,
    /// ... or this long when the last touch said the client was paused (`TC_ORPHAN_PAUSED_SECS`).
    pub orphan_paused: Duration,
    /// Hard cap on how long a detached job (or its post-exit cleanup wait) lives
    /// (`TC_ORPHAN_MAX_SECS`).
    pub orphan_max: Duration,
}

fn env(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// A boolean env var: `0`/`false`/`no`/`off` (case-insensitive) are off, anything else present is
/// on, unset is `default`. Unlike `TC_HW_FILTERS`'s bare `!= "0"` (on by default, rarely turned
/// off), `TC_ACCEPT_BATCH` exists specifically to be turned off on one worker, so a typo'd value
/// must not silently read as "on".
fn env_bool(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => !matches!(
            v.to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        _ => default,
    }
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
            // Default is minutes, not the PLAYBACK-tuned tens-of-seconds: trickplay's `time=`
            // cadence is one muxed frame per ~10 source-seconds (fps=0.1), and A5's opt-in
            // CPU-spill worker decodes single-threaded, so 10 source-seconds of a 4K/HDR source
            // can genuinely take minutes. 300s covers roughly 0.033x realtime decode.
            batch_stall_after: Duration::from_secs_f64(env_f64("TC_BATCH_STALL_AFTER", 300.0)),
            batch_first_progress_grace: Duration::from_secs_f64(env_f64(
                "TC_BATCH_FIRST_PROGRESS_GRACE",
                300.0,
            )),
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
                    // A2: the shared scratch mount workers must write trickplay frames under
                    // (Jellyfin's TempDirectory, not its final sprite-sheet directory). Unset
                    // (None) fails closed: validate() then refuses every trickplay job.
                    trickplay_output_root: std::env::var("TC_TRICKPLAY_OUTPUT_ROOT")
                        .ok()
                        .filter(|v| !v.is_empty()),
                }
            },
            batch_weight: env_f64("TC_BATCH_WEIGHT", 1.0),
            accept_batch: env_bool("TC_ACCEPT_BATCH", true),
            batch_headroom: env_f64("TC_BATCH_HEADROOM", 0.0),
            detach: env_bool("TC_DETACH", true),
            // Match Jellyfin's own kill timer (60 s HLS, 180 s paused with bughunt patch 02).
            orphan_idle: Duration::from_secs_f64(env_f64("TC_ORPHAN_IDLE_SECS", 60.0)),
            orphan_paused: Duration::from_secs_f64(env_f64("TC_ORPHAN_PAUSED_SECS", 180.0)),
            orphan_max: Duration::from_secs_f64(env_f64("TC_ORPHAN_MAX_SECS", 6.0 * 3600.0)),
        })
    }

    /// A minimal `Config` for unit tests, overridden field-by-field as each test needs.
    #[cfg(test)]
    pub fn minimal() -> Config {
        Config {
            name: "test".into(),
            node: String::new(),
            backend: Backend::Cpu,
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
            port: 9901,
            metrics_port: None,
            pathmap: vec![],
            capacity: 4.0,
            weight_1440: 1.0,
            weight_4k: 1.0,
            weight_copy: 1.0,
            outputs_allow: None,
            gpu_filters: false,
            probe_clamp: None,
            fence_after: Duration::from_secs(3),
            stall_after: Duration::from_secs(20),
            first_progress_grace: Duration::from_secs(45),
            batch_stall_after: Duration::from_secs(300),
            batch_first_progress_grace: Duration::from_secs(300),
            policy: tcpool_ir::validate::Policy::default(),
            batch_weight: 1.0,
            accept_batch: true,
            batch_headroom: 0.0,
            detach: true,
            orphan_idle: Duration::from_secs(60),
            orphan_paused: Duration::from_secs(180),
            orphan_max: Duration::from_secs(6 * 3600),
        }
    }

    /// Units for a source of `height` pixels (`None` = unknown -> the most expensive weight).
    pub fn weight_for_height(&self, height: Option<u32>) -> f64 {
        match height {
            Some(h) if h <= 1100 => 1.0, // <= 1080p, incl. 1920x1080 with bars
            Some(h) if h <= 1700 => self.weight_1440,
            _ => self.weight_4k,
        }
    }

    /// `(stall_after, first_progress_grace)` for a job of this class. BATCH (trickplay) jobs use
    /// their own, much longer pair -- see `batch_stall_after`'s doc comment for why the
    /// PLAYBACK-tuned defaults are unsafe to reuse for a `fps=0.1`-cadence, single-threaded decode.
    pub fn stall_limits(&self, batch: bool) -> (Duration, Duration) {
        if batch {
            (self.batch_stall_after, self.batch_first_progress_grace)
        } else {
            (self.stall_after, self.first_progress_grace)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A unique var name per test (not shared with any real TC_* knob) avoids racing other tests
    // over process-global env state.
    fn set(name: &str, v: &str) {
        std::env::set_var(name, v);
    }
    fn unset(name: &str) {
        std::env::remove_var(name);
    }

    #[test]
    fn env_bool_recognises_false_shapes_case_insensitively() {
        let var = "TCPOOL_TEST_ENV_BOOL_FALSE";
        for v in ["0", "false", "FALSE", "no", "NO", "off", "Off"] {
            set(var, v);
            assert!(!env_bool(var, true), "{v:?} must read as off");
        }
        unset(var);
    }

    #[test]
    fn env_bool_defaults_when_unset_and_is_on_for_anything_else() {
        let var = "TCPOOL_TEST_ENV_BOOL_DEFAULT";
        unset(var);
        assert!(env_bool(var, true));
        assert!(!env_bool(var, false));
        set(var, "1");
        assert!(env_bool(var, false));
        set(var, "yes");
        assert!(env_bool(var, false));
        unset(var);
    }

    #[test]
    fn stall_limits_picks_the_batch_pair_and_it_is_materially_longer() {
        let mut cfg = Config::minimal();
        cfg.stall_after = Duration::from_secs(20);
        cfg.first_progress_grace = Duration::from_secs(45);
        cfg.batch_stall_after = Duration::from_secs(300);
        cfg.batch_first_progress_grace = Duration::from_secs(300);
        assert_eq!(
            cfg.stall_limits(false),
            (Duration::from_secs(20), Duration::from_secs(45))
        );
        let (batch_stall, batch_grace) = cfg.stall_limits(true);
        assert_eq!(batch_stall, Duration::from_secs(300));
        assert_eq!(batch_grace, Duration::from_secs(300));
        // The point of the regression: a BATCH job decoding at the ~0.5x-realtime boundary the
        // review's repro used (10 source-seconds needing >20 wall-seconds) must not fit inside
        // the PLAYBACK-tuned default, but must comfortably fit inside the BATCH one.
        let ten_source_seconds_at_half_realtime = Duration::from_secs(20);
        assert!(ten_source_seconds_at_half_realtime >= cfg.stall_after);
        assert!(ten_source_seconds_at_half_realtime < batch_stall);
    }
}
