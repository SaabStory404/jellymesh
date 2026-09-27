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
}
