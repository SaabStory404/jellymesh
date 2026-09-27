//! Model of the ffmpeg command lines Jellyfin emits in software mode (`hwaccel=none`), and their
//! translation to each worker backend.
//!
//! Translation is a *structured patch*: only the video encoder, rate control, the video filter
//! chain and hardware decode are rewritten; every other option is kept verbatim and in order.
//! The P1 target is byte-for-byte parity with the spike's `agent.translate()`
//! (`corpus/goldens/spike-translate.json`); deliberate improvements come after, as golden updates.

pub mod filters;
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
pub fn render(args: &[String], backend: Backend, o: &TranslateOpts) -> Translated {
    let mut t = if is_video_copy(args) {
        Translated {
            args: args.iter().map(|a| map_path(a, &o.pathmap)).collect(),
            gpu_filters: false,
        }
    } else {
        translate(args, backend, o)
    };
    add_hls_flag(&mut t.args, "temp_file");
    t
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
/// playback decision selects a DV7 source for a client that plays DV 8.1 (`transcode/docs/PLAN.md`
/// P5). Chosen because it is a real, harmless ffmpeg option (arbitrary output metadata): any
/// fallback path that execs ffmpeg with this argv unmodified (an older agent, or the shim's own
/// `exec_real` when the pool is unreachable) still runs correctly and just tags the output file
/// with one extra, inert metadata key -- it does not change what ffmpeg does. Detection is a
/// literal, adjacent-pair match, not a prefix/substring match, so no other stream-metadata tag can
/// collide with it by accident.
pub const DV81_SIGNAL_FLAG: &str = "-metadata:s:v:0";
pub const DV81_SIGNAL_VALUE: &str = "TC_DV81=1";

/// `true` when the Jellyfin-side decision patch asked for an on-the-fly DV7 -> 8.1 remux (see
/// `DV81_SIGNAL_FLAG`).
///
/// Detection only: `render()` does not act on this today, so the pool is a no-op for a signaled
/// job exactly like any other job (P5 is not wired into the agent's exec path yet -- see
/// `transcode/docs/PLAN.md` P5 for the measured blocker and what's needed before it can be). An
/// agent that does wire it must still confirm the *source* really is DV profile 7 (an ffprobe
/// check -- `probe::source_dovi_profile` -- not this signal alone) before doing anything
/// different: the signal only says what Jellyfin *wants*, never what the source *is*, so a signal
/// on a DV5/DV8/non-DV source must fall through unchanged rather than mislabel it.
pub fn wants_dv81(args: &[String]) -> bool {
    args.windows(2)
        .any(|w| w[0] == DV81_SIGNAL_FLAG && w[1] == DV81_SIGNAL_VALUE)
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
            "TC_DV81=1",
            "-codec:v:0",
            "libx264",
        ]);
        assert!(wants_dv81(&a));
    }

    #[test]
    fn wants_dv81_ignores_absence_and_near_misses() {
        assert!(!wants_dv81(&s(&["-i", "x", "-codec:v:0", "libx264"])));
        // wrong value
        assert!(!wants_dv81(&s(&["-metadata:s:v:0", "TC_DV81=0"])));
        // wrong stream index
        assert!(!wants_dv81(&s(&["-metadata:s:v:1", "TC_DV81=1"])));
        // a different metadata key that happens to contain the value string
        assert!(!wants_dv81(&s(&["-metadata:s:v:0", "title=TC_DV81=1"])));
        // the two tokens present but not adjacent
        assert!(!wants_dv81(&s(&[
            "-metadata:s:v:0",
            "title=x",
            "TC_DV81=1"
        ])));
    }

    #[test]
    fn render_does_not_yet_touch_the_dv81_signal() {
        // P5 is detection-only so far (see `wants_dv81`'s docs): render() must pass the signal
        // through byte-for-byte, same as any other unrecognised metadata option, until the agent
        // side actually wires the conversion.
        let a = s(&[
            "-i",
            "x",
            "-metadata:s:v:0",
            "TC_DV81=1",
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
}
