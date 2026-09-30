//! Model of the ffmpeg command lines Jellyfin emits in software mode (`hwaccel=none`), and their
//! translation to each worker backend.
//!
//! Translation is a *structured patch*: only the video encoder, rate control, the video filter
//! chain and hardware decode are rewritten; every other option is kept verbatim and in order.
//! The P1 target is byte-for-byte parity with the spike's `agent.translate()`
//! (`corpus/goldens/spike-translate.json`); deliberate improvements come after, as golden updates.

pub mod filters;
pub mod shared;
pub mod validate;

use std::fmt;

/// A worker backend kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    /// Intel Quick Sync (VA-API decode/filters, QSV encode). Arc A380.
    Qsv,
    /// NVIDIA NVENC (CUDA decode/filters). Tesla P4.
    Nvenc,
    /// Software: the command runs unchanged.
    Cpu,
}

impl Backend {
    pub fn parse(s: &str) -> Option<Backend> {
        match s {
            "qsv" => Some(Backend::Qsv),
            "nvenc" => Some(Backend::Nvenc),
            "cpu" => Some(Backend::Cpu),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Qsv => "qsv",
            Backend::Nvenc => "nvenc",
            Backend::Cpu => "cpu",
        }
    }

    /// Jellyfin's software encoder -> this backend's encoder.
    pub fn encoder_for(self, software: &str) -> Option<&'static str> {
        use Backend::*;
        Some(match (self, software) {
            (Nvenc, "libx264") => "h264_nvenc",
            (Nvenc, "libx265") => "hevc_nvenc",
            (Nvenc, "libsvtav1") => "av1_nvenc",
            (Qsv, "libx264") => "h264_qsv",
            (Qsv, "libx265") => "hevc_qsv",
            (Qsv, "libsvtav1") => "av1_qsv",
            (Cpu, "libx264") => "libx264",
            (Cpu, "libx265") => "libx265",
            (Cpu, "libsvtav1") => "libsvtav1",
            _ => return None,
        })
    }

    /// Hardware device init + decode, inserted before the first `-i`.
    pub fn hwaccel_args(self) -> &'static [&'static str] {
        match self {
            Backend::Nvenc => &["-hwaccel", "cuda"],
            // Jellyfin's own QSV device init (prod FFmpeg.Transcode log, Arc A380)
            Backend::Qsv => &[
                "-init_hw_device",
                "vaapi=va:/dev/dri/renderD128,driver=iHD",
                "-init_hw_device",
                "qsv=qs@va",
                "-filter_hw_device",
                "qs",
                "-hwaccel",
                "vaapi",
            ],
            Backend::Cpu => &[],
        }
    }

    /// The device-init prefix of `hwaccel_args` (no `-hwaccel`), for probes.
    pub fn device_init_args(self) -> &'static [&'static str] {
        let a = self.hwaccel_args();
        match a.iter().position(|x| *x == "-hwaccel") {
            Some(i) => &a[..i],
            None => a,
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// x264/x265 presets -> NVENC p1 (fastest) .. p7 (slowest).
fn nv_preset(p: &str) -> Option<&'static str> {
    Some(match p {
        "ultrafast" | "superfast" => "p1",
        "veryfast" => "p2",
        "faster" => "p3",
        "fast" | "medium" => "p4",
        "slow" => "p5",
        "slower" => "p6",
        "veryslow" => "p7",
        _ => return None,
    })
}

fn is_video_codec_flag(a: &str) -> bool {
    a.starts_with("-codec:v") || a.starts_with("-c:v") || a == "-vcodec"
}

/// Options that tune a translation.
#[derive(Debug, Clone, Default)]
pub struct TranslateOpts {
    /// `(jellyfin prefix, local path)` replacements applied to every argument (incl. filters).
    pub pathmap: Vec<(String, String)>,
    /// Keep scale/tonemap on the GPU when the chain is translatable.
    pub gpu_filters: bool,
    /// Rate-control mapping `render()` applies on top of the parity translation.
    pub rate_control: RateControl,
}

/// How `render()` maps Jellyfin's `-crf` + `-maxrate`/`-bufsize` onto a hardware encoder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RateControl {
    /// P5: bitrate-targeted rate control per encoder from the calibration
    /// (`docs/engineering/transcode-calibration.md`), so the delivered bitrate tracks the client's cap and the two
    /// cards land at the same quality. See `apply_rate_control`.
    #[default]
    Calibrated,
    /// The P1 parity mapping: `-crf N` -> `-global_quality N` (QSV, which the iHD driver runs as
    /// CQP: cap-blind, 16-22% of the cap MEASURED) / `-cq N` (NVENC). Rollback only.
    Legacy,
}

impl RateControl {
    /// `calibrated` | `legacy` (the agent's `TC_RC`).
    pub fn parse(s: &str) -> Option<RateControl> {
        match s {
            "calibrated" => Some(RateControl::Calibrated),
            "legacy" => Some(RateControl::Legacy),
            _ => None,
        }
    }
}

/// Result of translating one command line for one backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Translated {
    pub args: Vec<String>,
    /// True when the video filter chain was moved to the GPU.
    pub gpu_filters: bool,
}

/// Apply the path map to one argument.
pub fn map_path(a: &str, pathmap: &[(String, String)]) -> String {
    let mut s = a.to_string();
    for (src, dst) in pathmap {
        s = s.replace(src.as_str(), dst);
    }
    s
}

/// Adapt Jellyfin's software command line to `backend`.
pub fn translate(args: &[String], backend: Backend, o: &TranslateOpts) -> Translated {
    let args: Vec<String> = args.iter().map(|a| map_path(a, &o.pathmap)).collect();
    if backend == Backend::Cpu {
        return Translated {
            args,
            gpu_filters: false,
        };
    }
    let hw_vf = if o.gpu_filters {
        args.iter()
            .position(|a| a == "-vf")
            .and_then(|i| args.get(i + 1))
            .and_then(|vf| filters::hw_filters(vf, backend))
    } else {
        None
    };
    let mut out: Vec<String> = Vec::with_capacity(args.len() + 16);
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let nxt = args.get(i + 1);
        if is_video_codec_flag(a) {
            let v = nxt.map(String::as_str).unwrap_or("");
            out.push(a.clone());
            if let Some(n) = nxt {
                out.push(
                    backend
                        .encoder_for(v)
                        .map(str::to_string)
                        .unwrap_or_else(|| n.clone()),
                );
            }
            i += 2;
            continue;
        }
        if backend == Backend::Nvenc && a.starts_with("-preset") {
            if let Some(p) = nxt.and_then(|n| nv_preset(n)) {
                out.push(a.clone());
                out.push(p.to_string());
                i += 2;
                continue;
            }
        }
        if let Some(suffix) = a.strip_prefix("-crf") {
            let flag = if backend == Backend::Nvenc {
                "-cq"
            } else {
                "-global_quality"
            };
            out.push(format!("{flag}{suffix}"));
            if let Some(n) = nxt {
                out.push(n.clone());
            }
            i += 2;
            continue;
        }
        if ["-x264opts", "-x264-params", "-x265-params", "-tune"]
            .iter()
            .any(|p| a.starts_with(p))
        {
            i += 2; // encoder-private options the hardware encoders do not accept
            continue;
        }
        if a == "-vf" {
            if let Some(vf) = &hw_vf {
                out.push(a.clone());
                out.push(vf.clone());
                i += 2;
                continue;
            }
        }
        out.push(a.clone());
        i += 1;
    }
    // Jellyfin forces a keyframe every segment and its playlist + restart logic assume segment N
    // starts at N*3 s; hardware encoders honour that only with forced IDR (MEASURED: NVENC cut
    // 10.46 s segments without it).
    if let Some(enc_at) = out.iter().position(|a| is_video_codec_flag(a)) {
        let idr = if backend == Backend::Nvenc {
            "-forced-idr"
        } else {
            "-forced_idr"
        };
        let at = (enc_at + 2).min(out.len());
        out.splice(at..at, [idr.to_string(), "1".to_string()]);
    }
    // Hardware decode; with GPU filters frames stay on the GPU, otherwise they are downloaded so
    // the CPU filter chain works.
    if let Some(first_input) = out.iter().position(|a| a == "-i") {
        let mut dec: Vec<String> = backend
            .hwaccel_args()
            .iter()
            .map(|s| s.to_string())
            .collect();
        if hw_vf.is_some() {
            dec.push("-hwaccel_output_format".into());
            dec.push(
                if backend == Backend::Qsv {
                    "vaapi"
                } else {
                    "cuda"
                }
                .into(),
            );
        }
        out.splice(first_input..first_input, dec);
    }
    Translated {
        args: out,
        gpu_filters: hw_vf.is_some(),
    }
}

/// True when the video stream is copied (no decode/encode): costs I/O, not GPU.
pub fn is_video_copy(args: &[String]) -> bool {
    args.iter()
        .position(|a| is_video_codec_flag(a))
        .and_then(|i| args.get(i + 1))
        .is_some_and(|c| c == "copy")
}

/// The production render: the parity translation plus deliberate improvements over the spike.
///
/// 1. `-hls_flags temp_file`: ffmpeg writes `<seg>.ts.tmp` and renames it when complete
///    (MEASURED on jellyfin-ffmpeg 8.1.2). Jellyfin's segment scan only matches the final
///    extension and serves segment N once N+1 exists, so it can never serve a half-written or
///    stale-sized segment over the shared NFS scratch (seen once: `Content-Length mismatch
///    8404992 of 8388608` on a stream copy).
/// 2. Stream copies get no hardware decode and no forced-IDR option (nothing is decoded).
/// 3. With `RateControl::Calibrated` (the default), `apply_rate_control` (P5).
pub fn render(args: &[String], backend: Backend, o: &TranslateOpts) -> Translated {
    let mut t = if is_video_copy(args) {
        Translated {
            args: args.iter().map(|a| map_path(a, &o.pathmap)).collect(),
            gpu_filters: false,
        }
    } else {
        let mut t = translate(args, backend, o);
        if o.rate_control == RateControl::Calibrated {
            apply_rate_control(&mut t.args, backend);
        }
        t
    };
    add_hls_flag(&mut t.args, "temp_file");
    t
}

/// Target average bitrate as a percentage of the client's cap (`-maxrate`), QSV. MEASURED
/// P0/P5/P5.1: the Arc delivers 94-97% of the cap at 95%, never over.
pub const RC_TARGET_PCT: u64 = 95;
/// Same for NVENC. The P4 delivers 7-9% **above** its `-b:v` (P5.1 lab, sample-b, VBV loose:
/// 8183 kbps for 7600, 6492 for 6080, 4587 for 4275), so at 95% a binding 4M cap came out at
/// 99.7-101.3% of the cap (P0: up to 105%). At 90% it delivered 96.5-98.4% of a 4M cap.
pub const NVENC_TARGET_PCT: u64 = 90;

/// The target percentage for `backend` (see `RC_TARGET_PCT`, `NVENC_TARGET_PCT`).
pub fn target_pct(backend: Backend) -> u64 {
    if backend == Backend::Nvenc {
        NVENC_TARGET_PCT
    } else {
        RC_TARGET_PCT
    }
}
/// Calibrated preset per backend (calibration README recommendation). Single-job cost vs the old
/// `veryfast`/`p2` (MEASURED P0, min over titles at 8M): Arc 11.5x vs 11.1x realtime, P4 4.48x vs
/// 4.56x.
pub const QSV_PRESET: &str = "medium";
pub const NVENC_PRESET: &str = "p5";

/// Encoder options that follow the bitrate target. Only options the driver was MEASURED to honour
/// (calibration README, "What the drivers actually accept"):
/// - `h264_qsv` gets **no `-look_ahead_depth`**: it SIGSEGVs jellyfin-ffmpeg on the Arc every time.
/// - `-adaptive_i`/`-adaptive_b` are silently dropped by QSV, so they are never emitted.
/// - `hevc_nvenc` on Pascal rejects `-temporal-aq` and `-b_ref_mode middle`.
/// - NVENC gets **no `-spatial-aq`/`-temporal-aq`** (P5 lab, MEASURED): with AQ the P4 scored
///   0.7-1.8 VMAF lower at the same delivered bitrate on the 4K SDR title and missed the ±1.5
///   cross-card target there (h264 3M +2.66, hevc 8M +1.93); without AQ every SDR rung is within
///   ±1.26 of the Arc.
fn rc_options(encoder: &str) -> Option<&'static [&'static str]> {
    Some(match encoder {
        "h264_qsv" => &["-extbrc", "1"],
        "hevc_qsv" => &[
            "-extbrc",
            "1",
            "-look_ahead_depth",
            "40",
            "-b_strategy",
            "1",
        ],
        "h264_nvenc" => &[
            "-rc",
            "vbr",
            "-tune",
            "hq",
            "-multipass",
            "fullres",
            "-b_ref_mode",
            "middle",
        ],
        "hevc_nvenc" => &["-rc", "vbr", "-tune", "hq", "-multipass", "fullres"],
        _ => return None,
    })
}

/// `a` is option `base`, bare or with a stream specifier (`-maxrate`, `-maxrate:v:0`).
fn is_opt(a: &str, base: &str) -> bool {
    a == base || a.strip_prefix(base).is_some_and(|s| s.starts_with(':'))
}

/// P5 rate control, applied to `translate()`'s output for a GPU backend.
///
/// Jellyfin's `-crf N -maxrate cap -bufsize 2cap` became `-global_quality N` (QSV) / `-cq N`
/// (NVENC) in the parity translation. On the Arc that is CQP, which ignores the cap (MEASURED P0:
/// 16-22% of the cap, VMAF 87 at 8 Mbps, identical at every rung). This replaces the quality
/// target with a bitrate target of `target_pct(backend)`% of `-maxrate`, keeps Jellyfin's
/// `-maxrate`/`-bufsize` exactly (clients depend on them), adds `rc_options`, and sets the
/// calibrated preset.
///
/// Unchanged: the CPU backend (libx264/libx265 CRF + VBV already honours the cap; the
/// calibration's `-preset slow` CPU anchor runs 0.18-0.59x realtime, so it is not a playback
/// setting), commands without `-maxrate` (no cap to track), stream copies, and encoders with no
/// calibration (AV1). An existing `-b:v` is kept.
pub fn apply_rate_control(args: &mut Vec<String>, backend: Backend) {
    let quality = match backend {
        Backend::Qsv => "-global_quality",
        Backend::Nvenc => "-cq",
        Backend::Cpu => return,
    };
    let Some(enc_at) = args.iter().position(|a| is_video_codec_flag(a)) else {
        return;
    };
    let Some(opts) = args.get(enc_at + 1).and_then(|e| rc_options(e)) else {
        return;
    };
    let Some(cap) = args
        .iter()
        .position(|a| is_opt(a, "-maxrate"))
        .and_then(|i| args.get(i + 1))
        .and_then(|v| parse_size(v))
        .filter(|c| *c > 0)
    else {
        return;
    };
    let mut set: Vec<String> = Vec::with_capacity(opts.len() + 2);
    if !args.iter().any(|a| is_opt(a, "-b:v") || a == "-vb") {
        set.push("-b:v".into());
        set.push((cap.saturating_mul(target_pct(backend)) / 100).to_string());
    }
    set.extend(opts.iter().map(|s| s.to_string()));
    // The quality target goes; the rate-control set takes its slot (else follows -maxrate's value).
    let at = match args.iter().position(|a| is_opt(a, quality)) {
        Some(q) => {
            args.drain(q..(q + 2).min(args.len()));
            q
        }
        None => args
            .iter()
            .position(|a| is_opt(a, "-maxrate"))
            .map_or(args.len(), |m| (m + 2).min(args.len())),
    };
    args.splice(at..at, set);
    let preset = if backend == Backend::Qsv {
        QSV_PRESET
    } else {
        NVENC_PRESET
    };
    match args.iter().position(|a| is_opt(a, "-preset")) {
        Some(p) if p + 1 < args.len() => args[p + 1] = preset.to_string(),
        _ => {
            let enc_at = args
                .iter()
                .position(|a| is_video_codec_flag(a))
                .unwrap_or(0);
            let at = (enc_at + 2).min(args.len());
            args.splice(at..at, ["-preset".to_string(), preset.to_string()]);
        }
    }
}

/// Add `flag` to `-hls_flags`, or insert `-hls_flags flag` just before the output playlist.
pub fn add_hls_flag(args: &mut Vec<String>, flag: &str) {
    if let Some(i) = args.iter().position(|a| a == "-hls_flags") {
        if let Some(v) = args.get_mut(i + 1) {
            if !v.split('+').any(|f| f == flag) {
                v.push('+');
                v.push_str(flag);
            }
        }
        return;
    }
    if let Some(out) = args.iter().rposition(|a| a.ends_with(".m3u8")) {
        args.splice(out..out, ["-hls_flags".to_string(), flag.to_string()]);
    }
}

/// ffmpeg's size syntax (`5000000`, `50M`, `1G`, `200k`; decimal SI multiples) -> value.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last()? {
        'k' | 'K' => (&s[..s.len() - 1], 1_000),
        'M' => (&s[..s.len() - 1], 1_000_000),
        'G' => (&s[..s.len() - 1], 1_000_000_000),
        _ => (s, 1),
    };
    num.parse::<u64>().ok()?.checked_mul(mult)
}

/// Lower Jellyfin's input `-probesize`/`-analyzeduration` to at most `probesize` bytes /
/// `analyzeduration` microseconds. Never raises them.
///
/// Jellyfin passes `-analyzeduration 200M -probesize 1G` for remuxes. ffmpeg then reads up to
/// ~1 GB before starting, chasing sparse PGS subtitle streams it still fails to characterise
/// ("Could not find codec parameters ... unspecified size" even at 1G). The job maps only video
/// and audio, and those come out identical at 50M/5M (MEASURED 2026-09-27 on all three lab
/// titles: H.264/DTS-HD MA, HEVC Main10/AAC, DV HEVC/TrueHD Atmos). Over the tower's 1 GbE link
/// the 1G probe cost 8-12 s before the first segment; 50M/5M opens in 0.07 s.
/// Skipped when `-filter_complex` is present: graphical-subtitle burn-in overlays a PGS stream
/// and needs its parameters.
pub fn clamp_probe(args: &mut [String], probesize: u64, analyzeduration: u64) {
    if args.iter().any(|a| a == "-filter_complex") {
        return;
    }
    let first_input = args.iter().position(|a| a == "-i").unwrap_or(args.len());
    for i in 0..first_input.saturating_sub(1) {
        let limit = match args[i].as_str() {
            "-probesize" => probesize,
            "-analyzeduration" => analyzeduration,
            _ => continue,
        };
        if parse_size(&args[i + 1]).is_some_and(|v| v > limit) {
            args[i + 1] = limit.to_string();
        }
    }
}

/// What the shim routes: an HLS transcode (an `.m3u8` output and an input).
pub fn is_hls_transcode(args: &[String]) -> bool {
    args.iter().any(|a| a.ends_with(".m3u8")) && args.iter().any(|a| a == "-i")
}

/// The command shape, for routing and for `validate::validate`'s per-shape allowlist rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// An HLS transcode: `is_hls_transcode` (unchanged).
    Hls,
    /// Jellyfin's trickplay sprite extraction: `mjpeg` to a `%0Nd.jpg` pattern.
    Trickplay,
    /// Anything else (probes, `-encoders`, chapter images, subtitle extraction, ...): the shim
    /// execs it locally, unchanged.
    Other,
}

/// Classify a command line. `Hls` is checked first, so a (theoretical) argv matching both shapes
/// keeps today's HLS behavior.
pub fn classify(args: &[String]) -> Shape {
    if is_hls_transcode(args) {
        return Shape::Hls;
    }
    if is_trickplay(args) {
        return Shape::Trickplay;
    }
    Shape::Other
}

fn video_codec_value(args: &[String]) -> Option<&str> {
    args.iter()
        .position(|a| is_video_codec_flag(a))
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// `true` for a value shaped like image2's frame-number pattern (`%d` or `%0[1-9]d`) ending in
/// `.jpg`/`.jpeg` -- Jellyfin's trickplay output. The pattern must sit immediately before the
/// extension, matching every trickplay argv seen (`.../%08d.jpg`); this also excludes
/// chapter-image extraction, which writes a single fixed `<guid>.jpg` with no `%d` at all.
pub(crate) fn is_trickplay_pattern(s: &str) -> bool {
    let s = s.trim_matches('"');
    let stem = match s.strip_suffix(".jpg").or_else(|| s.strip_suffix(".jpeg")) {
        Some(stem) => stem,
        None => return false,
    };
    if stem.ends_with("%d") {
        return true;
    }
    let bytes = stem.as_bytes();
    if bytes.len() >= 4 {
        let tail = &bytes[bytes.len() - 4..];
        // %0Nd, N in 1..=9 (0 would be a no-op width, and image2 pads left with zeros anyway).
        if tail[0] == b'%'
            && tail[1] == b'0'
            && tail[2].is_ascii_digit()
            && tail[2] != b'0'
            && tail[3] == b'd'
        {
            return true;
        }
    }
    false
}

fn is_trickplay(args: &[String]) -> bool {
    args.iter().any(|a| a == "-i")
        && video_codec_value(args) == Some("mjpeg")
        && args.last().is_some_and(|a| is_trickplay_pattern(a))
}

/// The directory a trickplay job writes its frames into (the parent of the final `%0Nd.jpg`
/// pattern).
pub fn trickplay_output_dir(args: &[String]) -> Option<String> {
    let last = args.last()?.trim_matches('"');
    std::path::Path::new(last)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Replace a `%d`/`%0Nd` token with `n`, zero-padded to its declared width (`%d` gets no padding).
fn replace_frame_token(pattern: &str, n: u64) -> Option<String> {
    let pos = pattern.rfind('%')?;
    let rest = &pattern[pos + 1..];
    if let Some(after) = rest.strip_prefix('d') {
        return Some(format!("{}{n}{after}", &pattern[..pos]));
    }
    let bytes = rest.as_bytes();
    if bytes.len() >= 3
        && bytes[0] == b'0'
        && bytes[1].is_ascii_digit()
        && bytes[1] != b'0'
        && bytes[2] == b'd'
    {
        let width = (bytes[1] - b'0') as usize;
        return Some(format!("{}{n:0width$}{}", &pattern[..pos], &rest[3..]));
    }
    None
}

/// The first frame file a trickplay job writes: the output pattern with its `%0Nd`/`%d` token
/// replaced by `-start_number` (or image2's own default, `1` -- unlike HLS's `0`, and unlike
/// `first_segment` there is no `-start_number` in today's trickplay argv, but honour one if
/// Jellyfin ever emits it).
pub fn trickplay_first_frame(args: &[String]) -> Option<String> {
    let pattern = args.last()?.trim_matches('"');
    let start: u64 = value_of(args, "-start_number")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    replace_frame_token(pattern, start)
}

/// Adapt a trickplay command to `backend`. A parallel, narrow renderer -- not a `translate()`
/// extension -- so the HLS path (and the 201-golden parity test) is byte-identical by
/// construction, not by care: nothing here is reachable from `translate()`/`render()`.
///
/// - Hardware **decode only** (no `-hwaccel_output_format`): frames land in system memory, so the
///   untouched CPU `-vf` chain (`fps`/`setpts`/`scale`, none of which are in `filters::hw_filters`'s
///   `KNOWN` list -- there is no GPU chain to move them to) and the always-software `mjpeg` encoder
///   see exactly what they do today. This is the standard, widely-used ffmpeg shape
///   (`-hwaccel cuda -i ... <software filters/encoder>`); a worker whose hardware decoder rejects
///   a source (an unusual codec/profile) just exits non-zero before writing anything, and the
///   shim's contract (see `shim::run_batch`'s `batch_pool_failure`) reruns a failure with no
///   frame on disk locally regardless of *why* the worker failed, so there is no golden-path or
///   correctness risk from choosing hardware decode here.
/// - `-c:v mjpeg`, `-crf`/`-preset`, and `-forced_idr` are untouched: none apply to mjpeg, and
///   `translate()`'s forced-IDR splice is deliberately not reused (see the module docs above).
/// - `-stats` is inserted right after `-loglevel`'s value (defensively prepended if `-loglevel` is
///   ever absent): `-loglevel error` alone suppresses ffmpeg's `time=` progress line, which the
///   agent's stall watchdog keys on; `-stats` overrides that and is already flag-allowlisted.
pub fn render_trickplay(args: &[String], backend: Backend, o: &TranslateOpts) -> Translated {
    let mut out: Vec<String> = args.iter().map(|a| map_path(a, &o.pathmap)).collect();
    if let Some(first_input) = out.iter().position(|a| a == "-i") {
        let dec: Vec<String> = backend
            .hwaccel_args()
            .iter()
            .map(|s| s.to_string())
            .collect();
        out.splice(first_input..first_input, dec);
    }
    if !out.iter().any(|a| a == "-stats") {
        match out.iter().position(|a| a == "-loglevel") {
            Some(i) => out.insert((i + 2).min(out.len()), "-stats".to_string()),
            None => out.insert(0, "-stats".to_string()),
        }
    }
    Translated {
        args: out,
        gpu_filters: false,
    }
}

fn value_of<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// The file Jellyfin waits for: `-hls_segment_filename` with `%d` = `-start_number`.
pub fn first_segment(args: &[String]) -> Option<String> {
    let pattern = value_of(args, "-hls_segment_filename")?;
    let start: u64 = value_of(args, "-start_number")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Some(pattern.replacen("%d", &start.to_string(), 1))
}

/// `-hls_segment_filename` split around `%d`: (directory, file prefix, file suffix).
pub fn segment_pattern(args: &[String]) -> Option<(String, String, String)> {
    let pattern = value_of(args, "-hls_segment_filename")?;
    let p = std::path::Path::new(pattern);
    let dir = p.parent()?.to_string_lossy().into_owned();
    let name = p.file_name()?.to_string_lossy().into_owned();
    let (prefix, suffix) = name.split_once("%d")?;
    Some((dir, prefix.to_string(), suffix.to_string()))
}

/// Highest complete segment index on disk (with `temp_file`, final names are only complete).
pub fn last_segment_index(args: &[String]) -> Option<u64> {
    let (dir, prefix, suffix) = segment_pattern(args)?;
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.strip_prefix(prefix.as_str())?
                .strip_suffix(suffix.as_str())?
                .parse::<u64>()
                .ok()
        })
        .max()
}

/// The HLS playlist path (the output).
pub fn playlist(args: &[String]) -> Option<&str> {
    args.iter()
        .rev()
        .find(|a| a.ends_with(".m3u8"))
        .map(String::as_str)
}

/// The primary input path, without the `file:` prefix.
pub fn input_path(args: &[String]) -> Option<&str> {
    value_of(args, "-i").map(|s| s.strip_prefix("file:").unwrap_or(s))
}

/// The output token the job needs (`h264`, `hevc`, `hevc10`, `av1`, `av1-10`), or `None` for a
/// stream copy / unknown encoder. Matches the capability probe's vocabulary.
pub fn required_output(args: &[String]) -> Option<&'static str> {
    let codec = args
        .iter()
        .position(|a| a.starts_with("-codec:v") || a.starts_with("-c:v"))
        .and_then(|i| args.get(i + 1))?;
    let base = match codec.as_str() {
        "libx264" => "h264",
        "libx265" => "hevc",
        "libsvtav1" => "av1",
        _ => return None,
    };
    let ten = args.iter().any(|a| {
        ["yuv420p10", "p010", "main10"]
            .iter()
            .any(|m| a.contains(m))
    });
    Some(match (base, ten) {
        ("hevc", true) => "hevc10",
        ("av1", true) => "av1-10",
        (b, _) => b,
    })
}

/// The Jellyfin-side decision patch's signal for an on-the-fly DV profile 7 -> 8.1 remux: an
/// explicit per-stream output metadata tag, appended once on the video stream when Jellyfin's
/// playback decision selects a DV7 source for a client that plays DV 8.1 (`docs/engineering/transcode-plan.md`
/// P5). Chosen because it is a real, harmless ffmpeg option (arbitrary output metadata): any
/// fallback path that execs ffmpeg with this argv unmodified (an older agent, or the shim's own
/// `exec_real` when the pool is unreachable) still runs correctly and just tags the output file
/// with one extra, inert metadata key -- it does not change what ffmpeg does. Detection is a
/// literal, adjacent-pair match, not a prefix/substring match, so no other stream-metadata tag can
/// collide with it by accident.
pub const DV81_SIGNAL_FLAG: &str = "-metadata:s:v:0";
/// The canonical marker value: what the Jellyfin-side decision patch actually emits (jellymesh PR
/// #3, bug-hunt patch 13).
pub const DV81_SIGNAL_VALUE: &str = "JELLYMESH_DOVI_P7_TO_81=1";
/// An accepted alias for `DV81_SIGNAL_VALUE`: the pool's own original marker value, from before
/// the Jellyfin-side patch settled on its own name. Kept so lab recipes and older argvs built
/// against the earlier contract keep working; not the value new callers should emit.
pub const DV81_SIGNAL_VALUE_ALIAS: &str = "TC_DV81=1";

fn is_dv81_signal_value(v: &str) -> bool {
    v == DV81_SIGNAL_VALUE || v == DV81_SIGNAL_VALUE_ALIAS
}

/// `true` when the Jellyfin-side decision patch asked for an on-the-fly DV7 -> 8.1 remux (see
/// `DV81_SIGNAL_FLAG`; accepts either `DV81_SIGNAL_VALUE` or the `DV81_SIGNAL_VALUE_ALIAS`).
///
/// `render()` passes the signal through untouched; the agent (`job.rs`) acts on it, and must
/// still confirm the *source* really is DV profile 7 (an ffprobe check -- `probe::source_dovi` --
/// not this signal alone) before doing anything different: the signal only says what Jellyfin
/// *wants*, never what the source *is*, so a signal on a DV5/DV8/non-DV source falls through to a
/// DV-removed fallback rather than mislabel it (see `add_dv_removal_bsf`).
pub fn wants_dv81(args: &[String]) -> bool {
    args.windows(2)
        .any(|w| w[0] == DV81_SIGNAL_FLAG && is_dv81_signal_value(&w[1]))
}

/// `args` with every `DV81_SIGNAL_FLAG <value>` pair removed, for `<value>` either
/// `DV81_SIGNAL_VALUE` or `DV81_SIGNAL_VALUE_ALIAS` (other `-metadata:s:v:0` values are kept).
/// Every caller of this also needs `add_dv_removal_bsf` on whatever it runs instead of converting
/// (see that function's doc comment for why): stripping the marker alone is not enough to make a
/// fallback safe for the client Jellyfin built this argv for.
pub fn strip_dv81_signal(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        if args[i] == DV81_SIGNAL_FLAG && args.get(i + 1).is_some_and(|v| is_dv81_signal_value(v)) {
            i += 2;
            continue;
        }
        out.push(args[i].clone());
        i += 1;
    }
    out
}

/// ffmpeg's `dovi_rpu` bitstream filter option that strips every Dolby Vision RPU NAL and
/// configuration record, leaving plain HDR10 (the base layer, which a profile-7/8 source's BL
/// compatibility guarantees is a valid HDR10 stream on its own). CONFIRMED from source: jellyfin's
/// own `debian/patches/0061-add-remove-dovi-hdr10plus-bsf.patch` (present at jellyfin-ffmpeg tag
/// `v8.1.2-5`) adds `remove_dovi` to the `h265_metadata` bsf (ffmpeg's "hevc_metadata"); it deletes
/// the trailing RPU (`HEVC_NAL_UNSPEC62`) and EL (`HEVC_NAL_UNSPEC63`) units per access unit *and*
/// removes the `AV_PKT_DATA_DOVI_CONF` coded side data in `h265_metadata_init`, so the output
/// carries no DOVI record at all -- matching `dv81_plan::without_dovi_strip`'s long-standing
/// "Jellyfin's HDR10 fallback" bsf. This is a jellyfin-ffmpeg-only Debian patch, not in stock
/// ffmpeg: MEASURED 2026-09-27, this workstation's stock Fedora ffmpeg 8.1.3 has no such option
/// (`ffmpeg -h bsf=hevc_metadata` lists none; using it errors `Option 'remove_dovi' not found`).
/// `job::dv81_it`'s fallback tests probe for the option and skip (not fail) when it is absent, the
/// same way they skip when `ffmpeg`/`ffprobe`/libx265 are missing.
pub const DV_REMOVAL_BSF: &str = "hevc_metadata=remove_dovi=1";

/// `args` (a single-output ffmpeg argv, marker already stripped) with `DV_REMOVAL_BSF` merged into
/// the video bitstream filter chain: appended (comma-joined, never a second `-bsf:v`) to an
/// existing `-bsf:v`/`-bsf:v:N` value, or inserted as a new `-bsf:v` immediately before the output
/// path if none exists.
///
/// Why every P5 fallback needs this, not just the marker stripped: Jellyfin's decision patch only
/// emits the marker for a client it already confirmed accepts DV 8.1 *or* HDR10 -- never raw
/// profile 7. Any path that cannot actually produce a real 8.1 record (the RPU rewrite never ran,
/// or the source turned out not to be profile 7 at all) must not silently copy whatever DV the
/// source happens to carry: an untouched profile-7 dual-layer stream is not decodable by a
/// single-layer-only 8.1 client, and a non-P7 mismatch between Jellyfin's decision and this
/// agent's own ffprobe is exactly the case a fail-closed policy should not guess about either.
/// Stripping DV outright always yields the one thing every such client already accepts. Callers:
/// `job.rs`'s `run_dv81` fallbacks (`dv81_local_fallback_args`-equivalent for the agent) and the
/// shim's `exec_real` when the pool cannot be reached at all (`crates/shim/src/main.rs`).
pub fn add_dv_removal_bsf(args: &[String]) -> Vec<String> {
    let Some((out_path, body)) = args.split_last() else {
        return args.to_vec();
    };
    let mut out: Vec<String> = Vec::with_capacity(args.len() + 2);
    let mut merged = false;
    let mut i = 0;
    while i < body.len() {
        let a = &body[i];
        if !merged && (a == "-bsf:v" || a.starts_with("-bsf:v:")) {
            out.push(a.clone());
            let existing = body.get(i + 1).cloned().unwrap_or_default();
            out.push(if existing.is_empty() {
                DV_REMOVAL_BSF.to_string()
            } else {
                format!("{existing},{DV_REMOVAL_BSF}")
            });
            merged = true;
            i += 2;
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    if !merged {
        out.push("-bsf:v".to_string());
        out.push(DV_REMOVAL_BSF.to_string());
    }
    out.push(out_path.clone());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn first_segment_uses_start_number() {
        let a = s(&[
            "-i",
            "x",
            "-start_number",
            "38",
            "-hls_segment_filename",
            "/t/ab%d.ts",
            "/t/ab.m3u8",
        ]);
        assert_eq!(first_segment(&a).as_deref(), Some("/t/ab38.ts"));
        assert!(is_hls_transcode(&a));
    }

    #[test]
    fn render_adds_temp_file_before_output() {
        let a = s(&[
            "-i",
            "x",
            "-codec:v:0",
            "libx264",
            "-f",
            "hls",
            "-hls_segment_filename",
            "/t/a%d.ts",
            "-y",
            "/t/a.m3u8",
        ]);
        let r = render(&a, Backend::Qsv, &TranslateOpts::default());
        let n = r.args.len();
        assert_eq!(
            &r.args[n - 3..],
            &s(&["-hls_flags", "temp_file", "/t/a.m3u8"])[..]
        );
        let mut b = s(&["-i", "x", "-hls_flags", "independent_segments", "/t/a.m3u8"]);
        add_hls_flag(&mut b, "temp_file");
        assert_eq!(b[3], "independent_segments+temp_file");
        add_hls_flag(&mut b, "temp_file");
        assert_eq!(b[3], "independent_segments+temp_file");
    }

    #[test]
    fn copy_gets_no_hwaccel() {
        let a = s(&["-i", "x", "-codec:v:0", "copy", "-f", "hls", "/t/a.m3u8"]);
        let r = render(&a, Backend::Nvenc, &TranslateOpts::default());
        assert!(!r.args.iter().any(|x| x == "-hwaccel" || x == "-forced-idr"));
        assert!(is_video_copy(&a));
    }

    #[test]
    fn required_output_detects_ten_bit() {
        let a = s(&["-i", "x", "-codec:v:0", "libx265", "-vf", "format=p010le"]);
        assert_eq!(required_output(&a), Some("hevc10"));
        let c = s(&["-i", "x", "-codec:v:0", "copy"]);
        assert_eq!(required_output(&c), None);
    }

    fn trickplay_row(keyframe_only: bool, fps_flag: &[&str], out: &str) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        v.extend(s(&["-loglevel", "error", "-threads", "1"]));
        if keyframe_only {
            v.extend(s(&["-skip_frame", "nokey"]));
        }
        v.extend(s(&[
            "-i",
            "file:/media/movies/x.mkv",
            "-map",
            "0:0",
            "-an",
            "-sn",
        ]));
        let vf = if keyframe_only {
            "fps=0.1,scale=320:-2".to_string()
        } else {
            "setpts=N/23.976/TB,fps=0.1,scale=320:-2".to_string()
        };
        v.push("-vf".to_string());
        v.push(vf);
        v.extend(s(&["-threads", "1", "-c:v", "mjpeg", "-qscale:v", "4"]));
        v.extend(fps_flag.iter().map(|x| x.to_string()));
        v.extend(s(&["-f", "image2", out]));
        v
    }

    #[test]
    fn classify_recognises_trickplay_shapes_and_rejects_chapter_images() {
        let sdr = trickplay_row(false, &["-vsync", "0"], "/tmp/tp/a/%08d.jpg");
        assert_eq!(classify(&sdr), Shape::Trickplay);
        let keyframe_only =
            trickplay_row(true, &["-fps_mode", "passthrough"], "/tmp/tp/b/%08d.jpg");
        assert_eq!(classify(&keyframe_only), Shape::Trickplay);
        let hdr_4k = trickplay_row(false, &["-fps_mode", "passthrough"], "/tmp/tp/c/%08d.jpeg");
        assert_eq!(classify(&hdr_4k), Shape::Trickplay);
        let bare_pct_d = trickplay_row(false, &["-vsync", "0"], "/tmp/tp/d/%d.jpg");
        assert_eq!(classify(&bare_pct_d), Shape::Trickplay);

        // Existing HLS golden shape stays Hls, unchanged.
        let hls = s(&[
            "-i",
            "x",
            "-codec:v:0",
            "libx264",
            "-f",
            "hls",
            "-hls_segment_filename",
            "/t/a%d.ts",
            "-y",
            "/t/a.m3u8",
        ]);
        assert_eq!(classify(&hls), Shape::Hls);

        // Chapter images: -vframes 1 to a fixed <guid>.jpg, no %0Nd pattern -- the explicit
        // negative case (must not be mistaken for trickplay).
        let chapter = s(&[
            "-loglevel",
            "error",
            "-i",
            "file:/media/movies/x.mkv",
            "-map",
            "0:0",
            "-vframes",
            "1",
            "-c:v",
            "mjpeg",
            "-f",
            "image2",
            "/tmp/chapters/9f1c2b3a.jpg",
        ]);
        assert_eq!(classify(&chapter), Shape::Other);
    }

    #[test]
    fn trickplay_first_frame_defaults_to_start_1_and_honours_start_number() {
        let a = trickplay_row(false, &["-vsync", "0"], "/tmp/tp/%08d.jpg");
        assert_eq!(
            trickplay_first_frame(&a).as_deref(),
            Some("/tmp/tp/00000001.jpg")
        );
        let mut b = a.clone();
        let n = b.len();
        b.splice(n - 1..n - 1, s(&["-start_number", "5"]));
        assert_eq!(
            trickplay_first_frame(&b).as_deref(),
            Some("/tmp/tp/00000005.jpg")
        );
        let bare = trickplay_row(false, &["-vsync", "0"], "/tmp/tp/%d.jpg");
        assert_eq!(
            trickplay_first_frame(&bare).as_deref(),
            Some("/tmp/tp/1.jpg")
        );
    }

    #[test]
    fn trickplay_output_dir_is_the_pattern_parent() {
        let a = trickplay_row(false, &["-vsync", "0"], "/tmp/tp/guid1/%08d.jpg");
        assert_eq!(trickplay_output_dir(&a).as_deref(), Some("/tmp/tp/guid1"));
    }

    #[test]
    fn render_trickplay_leaves_mjpeg_and_vf_untouched_and_adds_stats_and_hwaccel() {
        let a = trickplay_row(false, &["-vsync", "0"], "/tmp/tp/%08d.jpg");
        let vf_before = a[a.iter().position(|x| x == "-vf").unwrap() + 1].clone();
        for backend in [Backend::Cpu, Backend::Qsv, Backend::Nvenc] {
            let r = render_trickplay(&a, backend, &TranslateOpts::default());
            assert!(!r.gpu_filters);
            assert_eq!(
                r.args[r.args.iter().position(|x| x == "-c:v").unwrap() + 1],
                "mjpeg"
            );
            assert!(!r
                .args
                .iter()
                .any(|x| x == "-forced_idr" || x == "-forced-idr"));
            let vf_after = &r.args[r.args.iter().position(|x| x == "-vf").unwrap() + 1];
            assert_eq!(vf_after, &vf_before, "backend {backend:?} touched -vf");
            assert_eq!(
                r.args.iter().filter(|x| *x == "-stats").count(),
                1,
                "backend {backend:?}: -stats must appear exactly once"
            );
            let has_hwaccel = r.args.iter().any(|x| x == "-hwaccel");
            match backend {
                Backend::Cpu => assert!(!has_hwaccel, "CPU must not get decode hwaccel args"),
                Backend::Qsv | Backend::Nvenc => {
                    assert!(has_hwaccel, "{backend:?} must get decode hwaccel args");
                    let i_pos = r.args.iter().position(|x| x == "-i").unwrap();
                    let hw_pos = r.args.iter().position(|x| x == "-hwaccel").unwrap();
                    assert!(hw_pos < i_pos, "hwaccel args must precede -i");
                }
            }
        }
    }

    #[test]
    fn render_trickplay_inserts_stats_after_loglevel_value_or_at_front() {
        let a = trickplay_row(false, &["-vsync", "0"], "/tmp/tp/%08d.jpg");
        let r = render_trickplay(&a, Backend::Cpu, &TranslateOpts::default());
        let ll = r.args.iter().position(|x| x == "-loglevel").unwrap();
        assert_eq!(r.args[ll + 2], "-stats");

        let no_loglevel = s(&[
            "-i",
            "file:/media/movies/x.mkv",
            "-vf",
            "fps=0.1",
            "-c:v",
            "mjpeg",
            "-f",
            "image2",
            "/tmp/tp/%08d.jpg",
        ]);
        let r2 = render_trickplay(&no_loglevel, Backend::Cpu, &TranslateOpts::default());
        assert_eq!(r2.args[0], "-stats");
    }

    #[test]
    fn probe_clamp_lowers_never_raises() {
        assert_eq!(parse_size("1G"), Some(1_000_000_000));
        assert_eq!(parse_size("200M"), Some(200_000_000));
        assert_eq!(parse_size("5000000"), Some(5_000_000));
        assert_eq!(parse_size("x"), None);
        let mut a = s(&[
            "-analyzeduration",
            "200M",
            "-probesize",
            "1G",
            "-i",
            "f",
            "-probesize",
            "9G",
        ]);
        clamp_probe(&mut a, 50_000_000, 5_000_000);
        assert_eq!(
            a,
            s(&[
                "-analyzeduration",
                "5000000",
                "-probesize",
                "50000000",
                "-i",
                "f",
                "-probesize",
                "9G"
            ])
        );
        let mut small = s(&["-probesize", "1M", "-i", "f"]);
        clamp_probe(&mut small, 50_000_000, 5_000_000);
        assert_eq!(small, s(&["-probesize", "1M", "-i", "f"]));
        let mut burn = s(&[
            "-probesize",
            "1G",
            "-i",
            "f",
            "-filter_complex",
            "[0:3]overlay",
        ]);
        clamp_probe(&mut burn, 50_000_000, 5_000_000);
        assert_eq!(burn[1], "1G");
    }

    #[test]
    fn wants_dv81_detects_the_exact_signal_pair() {
        let a = s(&[
            "-i",
            "file:/media/movies/x.mkv",
            "-metadata:s:v:0",
            "JELLYMESH_DOVI_P7_TO_81=1",
            "-codec:v:0",
            "libx264",
        ]);
        assert!(wants_dv81(&a));
    }

    #[test]
    fn wants_dv81_also_accepts_the_tc_dv81_alias() {
        // The pool's original marker value, from before the Jellyfin-side patch settled on its
        // own name (`DV81_SIGNAL_VALUE_ALIAS`). Lab recipes and older argvs still use it.
        let a = s(&[
            "-i",
            "file:/media/movies/x.mkv",
            "-metadata:s:v:0",
            "TC_DV81=1",
            "-codec:v:0",
            "libx264",
        ]);
        assert!(wants_dv81(&a));
    }

    #[test]
    fn wants_dv81_ignores_absence_and_near_misses() {
        assert!(!wants_dv81(&s(&["-i", "x", "-codec:v:0", "libx264"])));
        // wrong value (both the canonical marker and its alias)
        assert!(!wants_dv81(&s(&[
            "-metadata:s:v:0",
            "JELLYMESH_DOVI_P7_TO_81=0"
        ])));
        assert!(!wants_dv81(&s(&["-metadata:s:v:0", "TC_DV81=0"])));
        // wrong stream index
        assert!(!wants_dv81(&s(&[
            "-metadata:s:v:1",
            "JELLYMESH_DOVI_P7_TO_81=1"
        ])));
        // a different metadata key that happens to contain the value string
        assert!(!wants_dv81(&s(&[
            "-metadata:s:v:0",
            "title=JELLYMESH_DOVI_P7_TO_81=1"
        ])));
        // the two tokens present but not adjacent
        assert!(!wants_dv81(&s(&[
            "-metadata:s:v:0",
            "title=x",
            "JELLYMESH_DOVI_P7_TO_81=1"
        ])));
    }

    #[test]
    fn strip_dv81_signal_removes_only_the_exact_pair() {
        let a = s(&[
            "-i",
            "x",
            "-metadata:s:v:0",
            "JELLYMESH_DOVI_P7_TO_81=1",
            "-metadata:s:v:0",
            "title=keep",
            "-y",
            "/t/a.m3u8",
        ]);
        let stripped = strip_dv81_signal(&a);
        assert!(!wants_dv81(&stripped));
        assert_eq!(
            stripped,
            s(&[
                "-i",
                "x",
                "-metadata:s:v:0",
                "title=keep",
                "-y",
                "/t/a.m3u8"
            ])
        );
        let untouched = s(&["-i", "x", "-y", "/t/a.m3u8"]);
        assert_eq!(strip_dv81_signal(&untouched), untouched);
    }

    #[test]
    fn strip_dv81_signal_removes_the_alias_too() {
        let a = s(&["-i", "x", "-metadata:s:v:0", "TC_DV81=1", "-y", "/t/a.m3u8"]);
        let stripped = strip_dv81_signal(&a);
        assert!(!wants_dv81(&stripped));
        assert_eq!(stripped, s(&["-i", "x", "-y", "/t/a.m3u8"]));
    }

    #[test]
    fn render_does_not_touch_the_dv81_signal() {
        // render() passes the signal through byte-for-byte like any other metadata option; the
        // agent decides (and strips it) in job.rs.
        let a = s(&[
            "-i",
            "x",
            "-metadata:s:v:0",
            "JELLYMESH_DOVI_P7_TO_81=1",
            "-codec:v:0",
            "libx264",
            "-f",
            "hls",
            "-hls_segment_filename",
            "/t/a%d.ts",
            "-y",
            "/t/a.m3u8",
        ]);
        assert!(wants_dv81(&a));
        let r = render(&a, Backend::Qsv, &TranslateOpts::default());
        assert!(
            wants_dv81(&r.args),
            "the signal must survive render() unchanged"
        );
    }

    /// A realistic shape of the Jellyfin-side decision patch's actual argv (jellymesh PR #3,
    /// bug-hunt patch 13): the marker pair can appear anywhere relative to the other output
    /// options (never assume position), and the argv still carries `-bsf:v hevc_mp4toannexb` and
    /// a `-map 0:<N>` for video that isn't necessarily stream 0. Stripping must remove only the
    /// marker pair and leave everything else -- including the bsf and the map -- byte-for-byte.
    #[test]
    fn strip_dv81_signal_leaves_a_jellyfin_shaped_argv_otherwise_untouched() {
        let a = s(&[
            "-analyzeduration",
            "200M",
            "-probesize",
            "50M",
            "-f",
            "mpegts",
            "-i",
            "file:/data/media/movies/X/X.ts",
            "-map_metadata",
            "-1",
            "-map_chapters",
            "-1",
            "-map",
            "0:2",
            "-map",
            "0:1",
            "-codec:v:0",
            "copy",
            "-bsf:v",
            "hevc_mp4toannexb",
            "-metadata:s:v:0",
            "JELLYMESH_DOVI_P7_TO_81=1",
            "-tag:v:0",
            "hvc1",
            "-codec:a:0",
            "libfdk_aac",
            "-copyts",
            "-avoid_negative_ts",
            "disabled",
            "-f",
            "hls",
            "-hls_segment_type",
            "fmp4",
            "-hls_fmp4_init_filename",
            "abc-1.mp4",
            "-hls_segment_filename",
            "/transcodes/jf/abc%d.mp4",
            "-y",
            "/transcodes/jf/abc.m3u8",
        ]);
        assert!(wants_dv81(&a));
        assert!(is_video_copy(&a));
        let stripped = strip_dv81_signal(&a);
        assert!(!wants_dv81(&stripped));
        let mut expected = a.clone();
        let i = expected
            .windows(2)
            .position(|w| w == ["-metadata:s:v:0", "JELLYMESH_DOVI_P7_TO_81=1"])
            .unwrap();
        expected.drain(i..i + 2);
        assert_eq!(stripped, expected);
        // The bsf and the non-zero video map both survive stripping unchanged.
        assert!(stripped
            .windows(2)
            .any(|w| w == ["-bsf:v", "hevc_mp4toannexb"]));
        assert!(stripped.windows(2).any(|w| w == ["-map", "0:2"]));
    }

    #[test]
    fn add_dv_removal_bsf_inserts_a_new_flag_before_the_output_when_none_exists() {
        let a = s(&["-i", "x", "-codec:v:0", "copy", "-y", "/t/a.m3u8"]);
        let out = add_dv_removal_bsf(&a);
        assert_eq!(
            out,
            s(&[
                "-i",
                "x",
                "-codec:v:0",
                "copy",
                "-y",
                "-bsf:v",
                DV_REMOVAL_BSF,
                "/t/a.m3u8"
            ])
        );
    }

    #[test]
    fn add_dv_removal_bsf_merges_into_an_existing_chain_not_a_second_flag() {
        let a = s(&["-i", "x", "-bsf:v", "hevc_mp4toannexb", "-y", "/t/a.m3u8"]);
        let out = add_dv_removal_bsf(&a);
        assert_eq!(out.iter().filter(|x| *x == "-bsf:v").count(), 1);
        let i = out.iter().position(|x| x == "-bsf:v").unwrap();
        assert_eq!(out[i + 1], format!("hevc_mp4toannexb,{DV_REMOVAL_BSF}"));
        assert_eq!(out.last().unwrap(), "/t/a.m3u8");
    }

    /// Jellyfin's software-mode HLS command for `enc` at an 8 Mbps cap (lab-sw.tsv shape).
    fn jf_rc(enc: &str) -> Vec<String> {
        let crf = if enc == "libx264" { "23" } else { "28" };
        s(&[
            "-i",
            "/media/x.mkv",
            "-map",
            "0:0",
            "-codec:v:0",
            enc,
            "-preset",
            "veryfast",
            "-crf",
            crf,
            "-maxrate",
            "8000000",
            "-bufsize",
            "16000000",
            "-force_key_frames:0",
            "expr:gte(t,n_forced*3)",
            "-vf",
            "scale=1920:-2,format=yuv420p",
            "-f",
            "hls",
            "-hls_segment_filename",
            "/transcodes/a%d.ts",
            "-y",
            "/transcodes/a.m3u8",
        ])
    }

    fn render_rc(args: &[String], b: Backend, rc: RateControl) -> Vec<String> {
        render(
            args,
            b,
            &TranslateOpts {
                rate_control: rc,
                ..Default::default()
            },
        )
        .args
    }

    /// The video encoder's options: from the codec flag up to `-force_key_frames:0`.
    fn enc_opts(out: &[String]) -> Vec<String> {
        let a = out.iter().position(|x| x == "-codec:v:0").unwrap();
        let b = out.iter().position(|x| x == "-force_key_frames:0").unwrap();
        out[a..b].to_vec()
    }

    #[test]
    fn calibrated_rate_control_per_encoder_matches_the_calibration_table() {
        let cases: [(&str, Backend, &[&str]); 4] = [
            (
                "libx264",
                Backend::Qsv,
                &[
                    "-codec:v:0",
                    "h264_qsv",
                    "-forced_idr",
                    "1",
                    "-preset",
                    "medium",
                    "-b:v",
                    "7600000",
                    "-extbrc",
                    "1",
                    "-maxrate",
                    "8000000",
                    "-bufsize",
                    "16000000",
                ],
            ),
            (
                "libx265",
                Backend::Qsv,
                &[
                    "-codec:v:0",
                    "hevc_qsv",
                    "-forced_idr",
                    "1",
                    "-preset",
                    "medium",
                    "-b:v",
                    "7600000",
                    "-extbrc",
                    "1",
                    "-look_ahead_depth",
                    "40",
                    "-b_strategy",
                    "1",
                    "-maxrate",
                    "8000000",
                    "-bufsize",
                    "16000000",
                ],
            ),
            (
                "libx264",
                Backend::Nvenc,
                &[
                    "-codec:v:0",
                    "h264_nvenc",
                    "-forced-idr",
                    "1",
                    "-preset",
                    "p5",
                    "-b:v",
                    "7200000",
                    "-rc",
                    "vbr",
                    "-tune",
                    "hq",
                    "-multipass",
                    "fullres",
                    "-b_ref_mode",
                    "middle",
                    "-maxrate",
                    "8000000",
                    "-bufsize",
                    "16000000",
                ],
            ),
            (
                "libx265",
                Backend::Nvenc,
                &[
                    "-codec:v:0",
                    "hevc_nvenc",
                    "-forced-idr",
                    "1",
                    "-preset",
                    "p5",
                    "-b:v",
                    "7200000",
                    "-rc",
                    "vbr",
                    "-tune",
                    "hq",
                    "-multipass",
                    "fullres",
                    "-maxrate",
                    "8000000",
                    "-bufsize",
                    "16000000",
                ],
            ),
        ];
        for (enc, b, want) in cases {
            let out = render_rc(&jf_rc(enc), b, RateControl::Calibrated);
            assert_eq!(enc_opts(&out), s(want), "{enc} on {b}");
            assert!(
                !out.iter().any(|x| x == "-global_quality" || x == "-cq"),
                "{enc} on {b}: quality target left in {out:?}"
            );
            // the rest of the command is untouched: same tail as the legacy render
            let legacy = render_rc(&jf_rc(enc), b, RateControl::Legacy);
            let tail = |v: &[String]| {
                let i = v.iter().position(|x| x == "-force_key_frames:0").unwrap();
                v[i..].to_vec()
            };
            assert_eq!(tail(&out), tail(&legacy));
        }
    }

    #[test]
    fn h264_qsv_never_gets_look_ahead_depth_and_hevc_nvenc_never_gets_pascal_rejects() {
        for rc in [RateControl::Calibrated, RateControl::Legacy] {
            let q = render_rc(&jf_rc("libx264"), Backend::Qsv, rc);
            assert!(!q.iter().any(|x| x.starts_with("-look_ahead")), "{q:?}");
            let n = render_rc(&jf_rc("libx265"), Backend::Nvenc, rc);
            assert!(
                !n.iter().any(|x| x == "-temporal-aq" || x == "-b_ref_mode"),
                "{n:?}"
            );
            for out in [&q, &n] {
                assert!(!out.iter().any(|x| x == "-adaptive_i" || x == "-adaptive_b"));
            }
            // AQ measured worse on VMAF and cross-card consistency (rc_options' doc).
            let h = render_rc(&jf_rc("libx264"), Backend::Nvenc, rc);
            for out in [&n, &h] {
                assert!(!out.iter().any(|x| x.ends_with("-aq")), "{out:?}");
            }
        }
    }

    #[test]
    fn legacy_rate_control_is_the_parity_translation_byte_for_byte() {
        for enc in ["libx264", "libx265"] {
            for b in [Backend::Qsv, Backend::Nvenc, Backend::Cpu] {
                let a = jf_rc(enc);
                let mut want = translate(&a, b, &TranslateOpts::default()).args;
                add_hls_flag(&mut want, "temp_file");
                assert_eq!(render_rc(&a, b, RateControl::Legacy), want, "{enc} {b}");
            }
        }
    }

    #[test]
    fn calibrated_leaves_cpu_copy_uncapped_and_av1_alone() {
        for rc in [RateControl::Calibrated, RateControl::Legacy] {
            // CPU: Jellyfin's own x264/x265 CRF+VBV already honours the cap.
            let a = jf_rc("libx264");
            assert_eq!(
                render_rc(&a, Backend::Cpu, RateControl::Calibrated),
                render_rc(&a, Backend::Cpu, RateControl::Legacy)
            );
            // Stream copy.
            let c = s(&["-i", "x", "-codec:v:0", "copy", "-f", "hls", "/t/a.m3u8"]);
            for b in [Backend::Qsv, Backend::Nvenc] {
                assert_eq!(render_rc(&c, b, rc), render_rc(&c, b, RateControl::Legacy));
            }
        }
        // No -maxrate (no cap to track), or an uncalibrated encoder (AV1): the parity
        // translation, including its preset, stands.
        let mut no_cap = Vec::new();
        for enc in ["libx264", "libx265"] {
            let mut a = jf_rc(enc);
            let i = a.iter().position(|x| x == "-maxrate").unwrap();
            a.drain(i..i + 4); // -maxrate N -bufsize N
            no_cap.push((enc, a));
        }
        no_cap.push(("libsvtav1", jf_rc("libsvtav1")));
        for (enc, a) in no_cap {
            for b in [Backend::Qsv, Backend::Nvenc] {
                assert_eq!(
                    render_rc(&a, b, RateControl::Calibrated),
                    render_rc(&a, b, RateControl::Legacy),
                    "{enc} {b}"
                );
            }
        }
    }

    #[test]
    fn calibrated_keeps_an_existing_bitrate_and_inserts_a_missing_preset() {
        let mut a = jf_rc("libx264");
        // Jellyfin's bitrate form: -b:v instead of -crf, no -preset.
        let i = a.iter().position(|x| x == "-preset").unwrap();
        a.splice(i..i + 4, s(&["-b:v", "5000000"]));
        let out = render_rc(&a, Backend::Qsv, RateControl::Calibrated);
        assert_eq!(out.iter().filter(|x| *x == "-b:v").count(), 1);
        let bv = out.iter().position(|x| x == "-b:v").unwrap();
        assert_eq!(out[bv + 1], "5000000");
        let p = out.iter().position(|x| x == "-preset").unwrap();
        assert_eq!(out[p + 1], QSV_PRESET);
        let m = out.iter().position(|x| x == "-maxrate").unwrap();
        assert_eq!(&out[m + 2..m + 4], &s(&["-extbrc", "1"])[..]);
        assert_eq!(RateControl::parse("legacy"), Some(RateControl::Legacy));
        assert_eq!(
            RateControl::parse("calibrated"),
            Some(RateControl::Calibrated)
        );
        assert_eq!(RateControl::parse("cq"), None);
        assert_eq!(RateControl::default(), RateControl::Calibrated);
    }

    #[test]
    fn calibrated_output_passes_the_allowlist_unchanged() {
        // Every new option takes a value, so validate()'s structural scan accepts them with no
        // allowlist change; pin that for every calibrated encoder.
        let p = validate::Policy::default();
        for enc in ["libx264", "libx265"] {
            for b in [Backend::Qsv, Backend::Nvenc] {
                let out = render_rc(&jf_rc(enc), b, RateControl::Calibrated);
                assert_eq!(
                    validate::validate(&out, &p, Shape::Hls),
                    Ok(()),
                    "{enc} {b}"
                );
            }
        }
    }

    #[test]
    fn add_dv_removal_bsf_also_merges_into_a_stream_indexed_chain() {
        let a = s(&["-i", "x", "-bsf:v:0", "hevc_mp4toannexb", "-y", "/t/a.m3u8"]);
        let out = add_dv_removal_bsf(&a);
        let i = out.iter().position(|x| x == "-bsf:v:0").unwrap();
        assert_eq!(out[i + 1], format!("hevc_mp4toannexb,{DV_REMOVAL_BSF}"));
    }
}
