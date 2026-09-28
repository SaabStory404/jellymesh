//! P5.1 bitrate ladder: the most a calibrated GPU encode spends for a given output codec,
//! resolution and frame rate, however high the client's cap is.
//!
//! Jellyfin sends `-maxrate <client max bitrate>` (MEASURED prod 2026-09-28: `61599184` for a LAN
//! client, `-maxrate 0 -bufsize 0` for a request without PlaybackInfo). P5 targeted 95% of that
//! cap, i.e. ~58 Mbit/s for a 1080p LAN stream. The ladder caps the target at the rung where
//! the calibration's VMAF gains flatten (`calibration/README.md`, "P5.1: bitrate ladder").
//!
//! Rungs are **cap-equivalents**: the rung is the `-maxrate` the calibration ran at, and the
//! encoder target is `RC_TARGET_PCT`% of `min(cap, rung)`. So a job whose client cap exceeds the
//! 1080p h264 rung gets exactly the `-b:v` of the calibration's 8M row.

/// One rung per output size class: (class height, h264 rung, hevc rung), bits/s, <= 32 fps.
///
/// - **1080p h264 = 8M is MEASURED** (P5, 2026-09-27, Arc, 1080p output): 3->8M buys +1.84 /
///   +2.51 VMAF (sample-a / sample-b), 8->15M only +0.71 / +1.85 for nearly twice the bits
///   (0.10-0.26 VMAF per Mbit vs 0.37-0.50 below 8M); sample-a is already 97.3 at 8M.
/// - **hevc = 0.8 x h264 is MEASURED** as an equal-VMAF ratio: the hevc bitrate matching each
///   h264 8M score, interpolated between the hevc 3M and 8M rows, is 0.86 / 0.69 (Arc, sample-a /
///   sample-b) and 0.89 / 0.76 (P4) of the h264 bitrate, mean 0.80. 10-bit hevc uses the hevc row
///   (INHERITED: not measured separately).
/// - **Every other class is INHERITED scaling**, not measured: the 1080p rung scaled by
///   (pixels / 1920x1080)^0.75, the usual rule of thumb for how bitrate grows with area.
pub const LADDER: [(u32, u64, u64); 6] = [
    (360, 1_500_000, 1_200_000),
    (480, 2_500_000, 2_000_000),
    (720, 4_500_000, 3_600_000),
    (1080, 8_000_000, 6_400_000),
    (1440, 12_000_000, 9_600_000),
    (2160, 22_000_000, 17_600_000),
];

/// Above this frame rate the rung is multiplied by `HIGH_FPS_NUM / HIGH_FPS_DEN` (INHERITED:
/// 50/60 fps needs more bits per second but fewer per frame; not measured here).
pub const HIGH_FPS: f64 = 32.0;
pub const HIGH_FPS_NUM: u64 = 3;
pub const HIGH_FPS_DEN: u64 = 2;

/// What the agent's admission ffprobe knows about the source's video stream. Zero = unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SourceVideo {
    pub width: u32,
    pub height: u32,
    /// Frames per second (`avg_frame_rate`), 0.0 if unknown.
    pub fps: f64,
}

/// One side of a Jellyfin `scale=` filter.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Dim {
    /// A literal size.
    Fixed(u32),
    /// `min(max(...), N)`: the source size, capped at N.
    Bound(u32),
    /// `-1`, `-2`, `ow/a`, `oh*a`: derived from the other side and the aspect ratio.
    Auto,
    /// Source size (`iw`/`ih`, or an expression we do not model).
    Source,
}

fn dim(e: &str) -> Dim {
    let e = e.trim().trim_start_matches("w=").trim_start_matches("h=");
    if let Ok(n) = e.parse::<i64>() {
        return if n > 0 {
            Dim::Fixed(n as u32)
        } else {
            Dim::Auto
        };
    }
    if e.contains("ow/a") || e.contains("oh*a") || e.contains("oh/a") || e.contains("ow*a") {
        return Dim::Auto;
    }
    // Jellyfin: trunc(min(max(iw\,ih*a)\,1920)/2)*2 -> the number after the last separator
    // inside `min(`.
    if let Some(i) = e.find("min(") {
        let inner = &e[i..];
        let bound = inner
            .rsplit(',')
            .next()
            .map(|t| t.trim_start_matches('\\'))
            .and_then(|t| t.split(')').next())
            .and_then(|t| t.trim().parse::<u32>().ok());
        if let Some(n) = bound {
            return Dim::Bound(n);
        }
    }
    Dim::Source
}

/// Split on `sep` outside parentheses, keeping backslash escapes.
fn split_top(s: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start, mut esc) = (0i32, 0usize, false);
    for (i, c) in s.char_indices() {
        if esc {
            esc = false;
            continue;
        }
        match c {
            '\\' => esc = true,
            '(' => depth += 1,
            ')' => depth -= 1,
            c if c == sep && depth == 0 => {
                out.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// The main video chain's `scale=` (w, h) expressions, if any.
fn main_scale(args: &[String]) -> Option<(Dim, Dim)> {
    let chain = if let Some(vf) = value(args, "-vf") {
        vf.to_string()
    } else {
        let fc = value(args, "-filter_complex")?;
        // Jellyfin's burn-in graph: `[sub]` chain; `[0:0]...[main]`; overlay. The video is the
        // chain labelled `[main]`, else the whole graph.
        split_top(fc, ';')
            .into_iter()
            .find(|c| c.trim_end().ends_with("[main]"))
            .unwrap_or(fc)
            .to_string()
    };
    let f = split_top(&chain, ',')
        .into_iter()
        .rev()
        .find_map(|f| f.trim().strip_prefix("scale="))?;
    let mut p = split_top(f, ':').into_iter();
    let w = dim(p.next()?);
    let h = p.next().map_or(Dim::Auto, dim);
    Some((w, h))
}

fn value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// Output (width, height) of the video, from Jellyfin's scale filter and the source size.
/// Unknown source sides resolve to their bound (an upper bound: Jellyfin's `min(...)` never
/// upscales), and a missing aspect ratio is taken as 16:9. `None` when nothing bounds it.
pub fn output_size(args: &[String], src: Option<SourceVideo>) -> Option<(u32, u32)> {
    let s = src.filter(|s| s.width > 0 && s.height > 0);
    let (sw, sh) = s.map_or((None, None), |s| (Some(s.width), Some(s.height)));
    let side = |d: Dim, src: Option<u32>| -> Option<u32> {
        match d {
            Dim::Fixed(n) => Some(n),
            Dim::Bound(n) => Some(src.map_or(n, |v| v.min(n))),
            Dim::Source => src,
            Dim::Auto => None,
        }
    };
    let (wd, hd) = main_scale(args).unwrap_or((Dim::Source, Dim::Source));
    let (w, h) = (side(wd, sw), side(hd, sh));
    // aspect (w/h) from the source, else 16:9
    let (an, ad) = s.map_or((16u64, 9u64), |s| (s.width as u64, s.height as u64));
    match (w, h) {
        (Some(w), Some(h)) => Some((w, h)),
        (Some(w), None) => Some((w, (w as u64 * ad / an) as u32)),
        (None, Some(h)) => Some(((h as u64 * an / ad) as u32, h)),
        (None, None) => s.map(|s| (s.width, s.height)),
    }
}

/// Output frame rate: the source's, lowered by an explicit `-r`.
pub fn output_fps(args: &[String], src: Option<SourceVideo>) -> Option<f64> {
    let r = args
        .iter()
        .position(|a| a == "-r" || a.starts_with("-r:v"))
        .and_then(|i| args.get(i + 1))
        .and_then(|v| parse_rate(v));
    let s = src.map(|s| s.fps).filter(|f| *f > 0.0);
    match (s, r) {
        (Some(s), Some(r)) => Some(s.min(r)),
        (a, b) => a.or(b),
    }
}

/// `24000/1001` or `23.976` -> fps.
pub fn parse_rate(v: &str) -> Option<f64> {
    let f = match v.split_once('/') {
        Some((n, d)) => n.trim().parse::<f64>().ok()? / d.trim().parse::<f64>().ok()?,
        None => v.trim().parse().ok()?,
    };
    (f.is_finite() && f > 0.0).then_some(f)
}

/// 16:9-equivalent height of (w, h): scope 1920x800 counts as 1080.
fn class_height(w: u32, h: u32) -> u32 {
    h.max((w as u64 * 9 / 16) as u32)
}

/// The ladder rung (cap-equivalent, bits/s) for `encoder` at the job's output size and rate.
/// `None` for an encoder with no ladder row (AV1, copy, CPU encoders).
///
/// Unknown output size -> the top rung (never below what the job needs).
pub fn rung(encoder: &str, args: &[String], src: Option<SourceVideo>) -> Option<u64> {
    let hevc = if encoder.starts_with("h264_") {
        false
    } else if encoder.starts_with("hevc_") {
        true
    } else {
        return None;
    };
    let top = LADDER[LADDER.len() - 1];
    let row = match output_size(args, src) {
        Some((w, h)) => {
            let c = class_height(w, h);
            // ~6% slack so 1920x1088 or 1280x736 stay in their class
            LADDER
                .iter()
                .copied()
                .find(|(ch, _, _)| c <= ch + ch / 16)
                .unwrap_or(top)
        }
        None => top,
    };
    let mut r = if hevc { row.2 } else { row.1 };
    if output_fps(args, src).is_some_and(|f| f > HIGH_FPS) {
        r = r * HIGH_FPS_NUM / HIGH_FPS_DEN;
    }
    Some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }
    const JF: &str = r"setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709,scale=trunc(min(max(iw\,ih*a)\,1920)/2)*2:trunc(ow/a/2)*2,format=yuv420p";
    fn src(w: u32, h: u32, fps: f64) -> Option<SourceVideo> {
        Some(SourceVideo {
            width: w,
            height: h,
            fps,
        })
    }
    fn vf(v: &str) -> Vec<String> {
        s(&["-i", "x", "-vf", v, "-y", "/t/a.m3u8"])
    }

    #[test]
    fn jellyfin_width_bound_resolves_against_the_source() {
        let a = vf(JF);
        assert_eq!(output_size(&a, src(3840, 2160, 24.0)), Some((1920, 1080)));
        assert_eq!(output_size(&a, src(3840, 1600, 24.0)), Some((1920, 800)));
        assert_eq!(output_size(&a, src(1280, 720, 24.0)), Some((1280, 720)));
        // no probe: the bound, 16:9
        assert_eq!(output_size(&a, None), Some((1920, 1080)));
        let b = vf(&JF.replace("1920", "7680"));
        assert_eq!(output_size(&b, src(3840, 2160, 24.0)), Some((3840, 2160)));
    }

    #[test]
    fn dual_bound_fixed_and_auto_shapes() {
        let dual = r"scale=trunc(min(max(iw\,ih*a)\,1280)/2)*2:trunc(min(max(iw/a\,ih)\,720)/2)*2";
        assert_eq!(
            output_size(&vf(dual), src(1920, 1080, 24.0)),
            Some((1280, 720))
        );
        assert_eq!(
            output_size(&vf("scale=-1:1080:fast_bilinear"), src(3840, 2160, 24.0)),
            Some((1920, 1080))
        );
        assert_eq!(
            output_size(&vf("scale=1280:-2,format=yuv420p"), None),
            Some((1280, 720))
        );
        // no scale: the source; nothing known: None
        assert_eq!(
            output_size(&vf("format=yuv420p"), src(1920, 800, 24.0)),
            Some((1920, 800))
        );
        assert_eq!(output_size(&vf("format=yuv420p"), None), None);
    }

    #[test]
    fn filter_complex_uses_the_main_chain_not_the_subtitle_scale() {
        let fc = format!(
            "[0:3]scale,scale=-1:1080:fast_bilinear,crop,pad=max(1920\\,iw):max(1080\\,ih):(ow-iw)/2:(oh-ih)/2:black@0,crop=1920:1080[sub];[0:0]{}[main];[main][sub]overlay=eof_action=pass",
            JF.replace("1920", "1280")
        );
        let a = s(&["-i", "x", "-filter_complex", &fc, "-y", "/t/a.m3u8"]);
        assert_eq!(output_size(&a, src(1920, 1080, 24.0)), Some((1280, 720)));
    }

    #[test]
    fn rungs_per_codec_class_and_rate() {
        let a = vf(JF);
        let k = |e, sv| rung(e, &a, sv);
        assert_eq!(k("h264_qsv", src(3840, 2160, 23.976)), Some(8_000_000));
        assert_eq!(k("hevc_nvenc", src(3840, 2160, 24.0)), Some(6_400_000));
        assert_eq!(k("h264_nvenc", src(3840, 1600, 24.0)), Some(8_000_000));
        assert_eq!(k("h264_qsv", src(1280, 720, 24.0)), Some(4_500_000));
        assert_eq!(k("h264_qsv", src(1920, 1080, 59.94)), Some(12_000_000));
        assert_eq!(k("h264_qsv", None), Some(8_000_000));
        assert_eq!(k("av1_qsv", src(1920, 1080, 24.0)), None);
        assert_eq!(k("libx264", src(1920, 1080, 24.0)), None);
        // 416-wide (the prod maxrate-0 shape) lands in the smallest class
        let small = vf(&JF.replace("1920", "416"));
        assert_eq!(
            rung("h264_qsv", &small, src(3840, 2160, 24.0)),
            Some(1_500_000)
        );
        // unknown everything -> top rung; beyond 2160 -> top rung
        assert_eq!(
            rung("h264_qsv", &vf("format=yuv420p"), None),
            Some(22_000_000)
        );
        let big = vf(&JF.replace("1920", "7680"));
        assert_eq!(
            rung("hevc_qsv", &big, src(7680, 4320, 24.0)),
            Some(17_600_000)
        );
        // -r lowers the rate
        let mut r = vf(JF);
        r.splice(0..0, s(&["-r", "30"]));
        assert_eq!(rung("h264_qsv", &r, src(1920, 1080, 60.0)), Some(8_000_000));
    }

    #[test]
    fn parse_rate_forms() {
        assert_eq!(parse_rate("25"), Some(25.0));
        assert!((parse_rate("24000/1001").unwrap() - 23.976).abs() < 0.001);
        assert_eq!(parse_rate("0/0"), None);
        assert_eq!(parse_rate("x"), None);
    }
}
