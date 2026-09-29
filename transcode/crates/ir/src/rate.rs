//! Calibrated rate control and presets for the production render (PLAN §4.2, §4.3, §11).
//!
//! Every constant and gate here comes from the P0 calibration of this pool's two cards
//! (`transcode/calibration/README.md`, MEASURED 2026-09-26/27 in tc-lab on an Arc A380 / iHD
//! and a Tesla P4 / Pascal, jellyfin-ffmpeg 8.1.2); nothing is inherited from ffmpeg docs.
//!
//! The bug this fixes: Jellyfin's software dialect sends `-crf 23 -maxrate 7808000 -bufsize
//! 15616000`, and the spike translated `-crf` to QSV `-global_quality`, which the iHD driver
//! turns into **CQP** — a pure quality target with no bitrate goal, only clamped by maxrate.
//! The Arc therefore delivered 16–22% of the client's cap (the same 0.8–1.8 Mbps at every cap
//! rung; VMAF flat across the ladder). The calibrated mapping targets VBR at 95% of the cap
//! (Arc measured 96% of cap, 94–97%, never over) and keeps Jellyfin's `-maxrate`/`-bufsize`
//! values byte-for-byte: clients depend on those caps.
//!
//! Driver truths the renderer must honour (all MEASURED, see calibration/README.md "What the
//! drivers actually accept"):
//! - `h264_qsv` on the Arc **SIGSEGVs (exit -11) in every combination that sets
//!   `-look_ahead_depth`** — the static table never emits it for h264, and that bit must not be
//!   turned on without a probe that proves the driver fixed it.
//! - `adaptive_i`/`adaptive_b` are silently dropped by the Arc on both codecs, so the renderer
//!   never emits them.
//! - Pascal `hevc_nvenc` rejects `-temporal-aq` and `-b_ref_mode` (ffmpeg errors out); Pascal
//!   `h264_nvenc` accepts them. Both are gated on [`EncoderCaps`] so the startup probe can
//!   enable them per encoder.

use crate::{parse_size, Backend};

/// Encoder preset tier (PLAN §4.3 "preset scaling by headroom", §3.4 "emergency degrade").
///
/// The calibration matrix ran three presets per card (`veryfast`/`medium`/`veryslow` on QSV,
/// `p2`/`p5`/`p7` on NVENC; realtime cost of the slowest vs fastest: 1.24–1.51x, MEASURED),
/// so the knob is the tier, not a preset name. The headroom policy will set it per job at
/// admission ("slowest preset that still gives >= 1.5x realtime at the current load"); the
/// emergency degrade sets `Fastest`. The static default is `Calibrated`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PresetTier {
    /// Fastest calibrated preset (QSV `veryfast`, NVENC `p2`): the emergency-degrade render
    /// (PLAN §3.4).
    Fastest,
    /// The per-card calibrated preset (QSV `medium`, NVENC `p5`). The static default, and the
    /// preset the calibration's VMAF numbers were measured at.
    #[default]
    Calibrated,
    /// Slowest calibrated preset (QSV `veryslow`, NVENC `p7`): idle-pool quality scaling
    /// (PLAN §4.3).
    Slowest,
}

impl PresetTier {
    /// The QSV preset for this tier.
    pub fn qsv(self) -> &'static str {
        match self {
            PresetTier::Fastest => "veryfast",
            PresetTier::Calibrated => "medium",
            PresetTier::Slowest => "veryslow",
        }
    }

    /// The NVENC preset for this tier.
    pub fn nvenc(self) -> &'static str {
        match self {
            PresetTier::Fastest => "p2",
            PresetTier::Calibrated => "p5",
            PresetTier::Slowest => "p7",
        }
    }

    fn for_backend(self, backend: Backend) -> &'static str {
        match backend {
            Backend::Qsv => self.qsv(),
            _ => self.nvenc(),
        }
    }

    /// Parse a tier by name (`fastest`, `calibrated`, `slowest`), for the agent's env/config
    /// knob. [`Backend::parse`] style.
    pub fn parse(s: &str) -> Option<PresetTier> {
        match s {
            "fastest" => Some(PresetTier::Fastest),
            "calibrated" => Some(PresetTier::Calibrated),
            "slowest" => Some(PresetTier::Slowest),
            _ => None,
        }
    }
}

/// Which rate-control options this card's driver actually honours, per encoder, as the startup
/// probe measures them. QSV silently drops options it cannot honour, so acceptance is read from
/// a verbose parameter dump or a test encode, never from the exit code (calibration/README.md).
///
/// `false` always means "do not emit the option" — the safe direction for an unknown card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EncoderCaps {
    /// NVENC: `-temporal-aq` accepted (P4 Pascal: h264 yes, HEVC **no**).
    pub nvenc_temporal_aq: bool,
    /// NVENC: `-b_ref_mode` accepted (P4 Pascal: h264 yes, HEVC **no**).
    pub nvenc_b_ref_mode: bool,
    /// QSV: `-look_ahead_depth` accepted (Arc hevc yes, `LookAheadDepth: 40, BRefType:
    /// pyramid`; h264 **SIGSEGVs**). Must stay false for `h264_qsv` on this driver.
    pub qsv_look_ahead_depth: bool,
    /// QSV: `-extbrc` honoured (Arc h264/hevc; it is the only lookahead-family option
    /// `h264_qsv` on the Arc honours).
    pub qsv_extbrc: bool,
    /// QSV: `-b_strategy` honoured (Arc hevc, `GopRefDist` 4 -> 5; no effect on h264).
    pub qsv_b_strategy: bool,
}

impl EncoderCaps {
    /// The MEASURED driver truths for this pool's cards (calibration/README.md): the static
    /// table [`crate::render`] uses until the agent threads its probe results through
    /// [`RenderQuality`]. `adaptive_i`/`adaptive_b` are deliberately not represented: the Arc
    /// silently drops both on every codec, so the renderer never emits them.
    pub fn measured(backend: Backend, encoder: &str) -> EncoderCaps {
        match (backend, encoder) {
            (Backend::Nvenc, "h264_nvenc") => EncoderCaps {
                nvenc_temporal_aq: true,
                nvenc_b_ref_mode: true,
                ..EncoderCaps::default()
            },
            // Pascal HEVC rejects -temporal-aq and -b_ref_mode (MEASURED: ffmpeg errors out).
            (Backend::Nvenc, "hevc_nvenc") | (Backend::Nvenc, _) => EncoderCaps::default(),
            // Arc h264_qsv: extbrc only. look_ahead_depth SIGSEGVs; adaptive_i/b are dropped.
            (Backend::Qsv, "h264_qsv") => EncoderCaps {
                qsv_extbrc: true,
                ..EncoderCaps::default()
            },
            (Backend::Qsv, "hevc_qsv") => EncoderCaps {
                qsv_extbrc: true,
                qsv_look_ahead_depth: true,
                qsv_b_strategy: true,
                ..EncoderCaps::default()
            },
            // av1_qsv: outside the calibration matrix; emit nothing.
            (Backend::Qsv, _) | (Backend::Cpu, _) => EncoderCaps::default(),
        }
    }
}

/// The production render's quality knobs: the preset tier plus the encoder's probed caps.
///
/// This is the interface PLAN §4.3's headroom policy will drive: the scheduling agent sets
/// `preset` per job at admission (and §3.4's emergency degrade sets it to
/// [`PresetTier::Fastest`]), and the agent fills `caps` from its startup probe instead of the
/// static table. [`crate::render`] uses [`RenderQuality::calibrated`] until then.
#[derive(Debug, Clone, Copy)]
pub struct RenderQuality {
    pub preset: PresetTier,
    pub caps: EncoderCaps,
}

impl RenderQuality {
    /// The calibrated static default for this backend + encoder.
    pub fn calibrated(backend: Backend, encoder: &str) -> RenderQuality {
        RenderQuality {
            preset: PresetTier::default(),
            caps: EncoderCaps::measured(backend, encoder),
        }
    }
}

/// True when `a` is `flag`, or `flag` plus a Jellyfin-style stream specifier (`-crf:0`,
/// `-sc_threshold:v:0`). A value that merely starts with the flag text (`-cqp`) does not match.
fn flag_is(a: &str, flag: &str) -> bool {
    match a.strip_prefix(flag) {
        Some("") => true,
        Some(rest) => {
            rest.starts_with(':')
                && rest[1..]
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b':')
        }
        None => false,
    }
}

/// The value of the first `flag` argument, if present.
fn value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| flag_is(a, flag))
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// The video encoder argv will use: the value of the first `-codec:v`-style flag.
pub(crate) fn video_encoder(args: &[String]) -> Option<&str> {
    let i = args.iter().position(|a| crate::is_video_codec_flag(a))?;
    args.get(i + 1).map(String::as_str)
}

/// The encoders the calibration matrix covers. Anything else (av1, or a software encoder the
/// backend cannot map) keeps the spike translation: the measurements here say nothing about
/// those paths, and AV1 is not offered anyway (PLAN §7).
fn calibrated(backend: Backend, encoder: &str) -> bool {
    matches!(
        (backend, encoder),
        (Backend::Qsv, "h264_qsv" | "hevc_qsv") | (Backend::Nvenc, "h264_nvenc" | "hevc_nvenc")
    )
}

/// The calibrated rate-control options for `backend`/`encoder` (without the `-b:v` target,
/// which the caller emits first so it lands where Jellyfin's own rate-control options were).
fn rc_options(backend: Backend, encoder: &str, caps: &EncoderCaps) -> Vec<String> {
    let mut v = Vec::new();
    let mut push = |a: &str, b: &str| {
        v.push(a.to_string());
        v.push(b.to_string());
    };
    match backend {
        Backend::Qsv => {
            if caps.qsv_extbrc {
                push("-extbrc", "1");
            }
            if caps.qsv_look_ahead_depth {
                // Calibrated lookahead depth (Arc hevc_qsv dump: LookAheadDepth: 40).
                push("-look_ahead_depth", "40");
            }
            if caps.qsv_b_strategy {
                push("-b_strategy", "1");
            }
        }
        Backend::Nvenc => {
            push("-rc", "vbr");
            push("-tune", "hq");
            push("-multipass", "fullres");
            push("-spatial-aq", "1");
            if caps.nvenc_temporal_aq {
                push("-temporal-aq", "1");
            }
            if caps.nvenc_b_ref_mode {
                push("-b_ref_mode", "middle");
            }
        }
        Backend::Cpu => {}
    }
    let _ = encoder;
    v
}

/// Rewrite Jellyfin's `-crf`/`-b:v`/`-maxrate`/`-bufsize` into this encoder's calibrated rate
/// control, and the preset into `q`'s tier. Runs on the *translated* argv (the encoder name is
/// already the hardware one, and `-crf` has become `-global_quality`/`-cq`).
///
/// `-maxrate`/`-bufsize` are kept byte-for-byte; only the target bitrate is derived (95% of the
/// cap, and never above a `-b:v` Jellyfin itself sent). Without a `-maxrate` there is no cap to
/// honour, so the spike's quality mapping (`-global_quality`/`-cq`) is kept as-is.
pub(crate) fn apply(args: &mut Vec<String>, backend: Backend, encoder: &str, q: &RenderQuality) {
    if !calibrated(backend, encoder) {
        return;
    }
    // The VBR target: 95% of the client's cap, so the VBV ceiling stays inside it (MEASURED:
    // Arc 94-97% of cap, never over). If Jellyfin sent its own video bitrate (a per-user
    // limit), never raise it.
    let target = value(args, "-maxrate")
        .and_then(parse_size)
        .filter(|c| *c > 0)
        .map(|c| c / 20 * 19)
        .map(|t| value(args, "-b:v").and_then(parse_size).map_or(t, |v| t.min(v)))
        .map(|t| t.max(1));

    let mut out: Vec<String> = Vec::with_capacity(args.len() + 12);
    let mut rc_emitted = false;
    let mut saw_preset = false;
    let mut emit_rc = |out: &mut Vec<String>| {
        out.push("-b:v".to_string());
        out.push(target.unwrap_or_default().to_string());
        out.extend(rc_options(backend, encoder, &q.caps));
        rc_emitted = true;
    };
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let v = args.get(i + 1);
        if let Some(t) = target {
            // QSV: -global_quality is CQP and ignores the cap (the bug): the quality target
            // becomes the VBR target instead.
            if backend == Backend::Qsv && flag_is(a, "-global_quality") {
                emit_rc(&mut out);
                i += 2; // drop the pair
                continue;
            }
            // NVENC keeps its quality target (-cq, from Jellyfin's -crf): the calibrated hybrid
            // is a capped quality target that still fills the cap.
            if backend == Backend::Nvenc && flag_is(a, "-cq") {
                out.push(a.to_string());
                i += if v.is_some() {
                    out.push(v.unwrap().clone());
                    2
                } else {
                    1
                };
                emit_rc(&mut out);
                continue;
            }
            // A -b:v Jellyfin sent itself: clamp it into the calibrated target. If the rc block
            // already carries a -b:v, this one is redundant.
            if flag_is(a, "-b:v") {
                if !rc_emitted {
                    out.push(a.to_string());
                    i += if v.is_some() {
                        out.push(t.to_string());
                        2
                    } else {
                        1
                    };
                    emit_rc(&mut out);
                } else {
                    i += v.is_some() as usize + 1;
                }
                continue;
            }
            // Anchor of last resort: the rc block goes right before the cap it is derived from.
            if flag_is(a, "-maxrate") && !rc_emitted {
                emit_rc(&mut out);
            }
        }
        if flag_is(a, "-preset") {
            saw_preset = true;
            out.push(a.to_string());
            i += if v.is_some() {
                out.push(q.preset.for_backend(backend).to_string());
                2
            } else {
                1
            };
            continue;
        }
        out.push(a.to_string());
        i += 1;
    }
    // No -preset in the command (Jellyfin always sends one for libx264/x265; the corpus's
    // preset-less rows are stream copies): emit the calibrated preset next to the encoder.
    if !saw_preset {
        let at = out
            .iter()
            .position(|a| a == "-forced_idr" || a == "-forced-idr")
            .map(|i| (i + 2).min(out.len()))
            .or_else(|| {
                out.iter()
                    .position(crate::is_video_codec_flag)
                    .map(|i| (i + 2).min(out.len()))
            })
            .unwrap_or(out.len());
        out.splice(
            at..at,
            [
                "-preset".to_string(),
                q.preset.for_backend(backend).to_string(),
            ],
        );
    }
    *args = out;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{render, render_with, TranslateOpts};

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// A Jellyfin software-mode job, the corpus shape (subset): -crf + cap.
    fn job(encoder: &str, crf: &str) -> Vec<String> {
        s(&[
            "-i",
            "file:/media/movies/x.mkv",
            "-codec:v:0",
            encoder,
            "-preset",
            "veryfast",
            "-crf",
            crf,
            "-maxrate",
            "7808000",
            "-bufsize",
            "15616000",
            "-f",
            "hls",
            "-hls_segment_filename",
            "/transcodes/a%d.ts",
            "-y",
            "/transcodes/a.m3u8",
        ])
    }

    /// (flag, value) pairs of the rendered argv, for "contains" style assertions.
    fn pairs(args: &[String]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for i in 0..args.len().saturating_sub(1) {
            out.push((args[i].clone(), args[i + 1].clone()));
        }
        out
    }

    fn has(args: &[String], flag: &str, value: &str) -> bool {
        pairs(args).iter().any(|(f, v)| f == flag && v == value)
    }

    #[test]
    fn qsv_h264_gets_calibrated_vbr_and_extbrc() {
        let r = render(&job("libx264", "23"), Backend::Qsv, &TranslateOpts::default());
        // CQP is gone; the VBR target is 95% of the cap; the cap itself is untouched.
        assert!(!r.args.iter().any(|a| flag_is(a, "-global_quality")));
        assert!(has(&r.args, "-b:v", "7417600"));
        assert!(has(&r.args, "-maxrate", "7808000"));
        assert!(has(&r.args, "-bufsize", "15616000"));
        assert!(has(&r.args, "-extbrc", "1"));
        assert!(has(&r.args, "-preset", "medium"));
        // h264_qsv never gets look_ahead_depth (SIGSEGV on the Arc) or adaptive_i/adaptive_b.
        assert!(!r.args.iter().any(|a| a.starts_with("-look_ahead_depth")));
        assert!(!r.args.iter().any(|a| a.starts_with("-adaptive_i")));
        assert!(!r.args.iter().any(|a| a.starts_with("-adaptive_b")));
    }

    #[test]
    fn qsv_hevc_adds_the_measured_lookahead_options() {
        let r = render(&job("libx265", "28"), Backend::Qsv, &TranslateOpts::default());
        assert!(has(&r.args, "-b:v", "7417600"));
        assert!(has(&r.args, "-extbrc", "1"));
        assert!(has(&r.args, "-look_ahead_depth", "40"));
        assert!(has(&r.args, "-b_strategy", "1"));
    }

    #[test]
    fn nvenc_h264_gets_the_calibrated_hybrid() {
        let r = render(&job("libx264", "23"), Backend::Nvenc, &TranslateOpts::default());
        assert!(has(&r.args, "-cq", "23")); // the quality target survives
        assert!(has(&r.args, "-rc", "vbr"));
        assert!(has(&r.args, "-tune", "hq"));
        assert!(has(&r.args, "-b:v", "7417600"));
        assert!(has(&r.args, "-multipass", "fullres"));
        assert!(has(&r.args, "-spatial-aq", "1"));
        assert!(has(&r.args, "-temporal-aq", "1")); // Pascal h264 accepts it
        assert!(has(&r.args, "-b_ref_mode", "middle"));
        assert!(has(&r.args, "-preset", "p5"));
        assert!(has(&r.args, "-maxrate", "7808000"));
        assert!(has(&r.args, "-bufsize", "15616000"));
    }

    #[test]
    fn nvenc_hevc_gates_the_pascal_rejected_options() {
        let r = render(&job("libx265", "28"), Backend::Nvenc, &TranslateOpts::default());
        // Pascal HEVC rejects -temporal-aq/-b_ref_mode (MEASURED): the static caps omit them.
        assert!(!r.args.iter().any(|a| a.starts_with("-temporal-aq")));
        assert!(!r.args.iter().any(|a| a.starts_with("-b_ref_mode")));
        assert!(has(&r.args, "-rc", "vbr"));
        assert!(has(&r.args, "-spatial-aq", "1"));
        assert!(has(&r.args, "-cq", "28"));
        // The interface the agent's probe will drive: same encoder, caps overridden.
        let q = RenderQuality {
            preset: PresetTier::Calibrated,
            caps: EncoderCaps {
                nvenc_temporal_aq: true,
                nvenc_b_ref_mode: true,
                ..EncoderCaps::default()
            },
        };
        let r = render_with(&job("libx265", "28"), Backend::Nvenc, &TranslateOpts::default(), &q);
        assert!(has(&r.args, "-temporal-aq", "1"));
        assert!(has(&r.args, "-b_ref_mode", "middle"));
    }

    #[test]
    fn no_cap_keeps_the_spike_quality_mapping() {
        let mut a = job("libx264", "23");
        let n = a.len();
        a.splice(n - 9..n - 7, []); // drop -maxrate/-bufsize
        let q = render(&a, Backend::Qsv, &TranslateOpts::default());
        assert!(has(&q.args, "-global_quality", "23"));
        assert!(!q.args.iter().any(|a| a == "-b:v"));
        let n = render(&a, Backend::Nvenc, &TranslateOpts::default());
        assert!(has(&n.args, "-cq", "23"));
        // The calibrated preset still applies.
        assert!(has(&q.args, "-preset", "medium"));
        assert!(has(&n.args, "-preset", "p5"));
    }

    #[test]
    fn explicit_b_v_is_never_raised() {
        let mut a = job("libx264", "23");
        let n = a.len();
        a.splice(
            n - 9..n - 9,
            s(&["-b:v".to_string(), "3000000".to_string()]),
        );
        let r = render(&a, Backend::Qsv, &TranslateOpts::default());
        assert!(has(&r.args, "-b:v", "3000000"));
        assert!(!r.args.iter().any(|a| flag_is(a, "-global_quality")));
    }

    #[test]
    fn capsized_maxrate_suffix_is_parsed_but_kept_verbatim() {
        let mut a = job("libx264", "23");
        let i = a.iter().position(|x| x == "-maxrate").unwrap();
        a[i + 1] = "7808k".into();
        let r = render(&a, Backend::Qsv, &TranslateOpts::default());
        assert!(has(&r.args, "-b:v", "7417600"));
        assert!(has(&r.args, "-maxrate", "7808k"));
    }

    #[test]
    fn preset_tiers_map_to_the_calibration_rungs() {
        for (tier, qsv, nvenc) in [
            (PresetTier::Fastest, "veryfast", "p2"),
            (PresetTier::Calibrated, "medium", "p5"),
            (PresetTier::Slowest, "veryslow", "p7"),
        ] {
            let q = RenderQuality {
                preset: tier,
                caps: EncoderCaps::measured(Backend::Qsv, "h264_qsv"),
            };
            let r = render_with(&job("libx264", "23"), Backend::Qsv, &TranslateOpts::default(), &q);
            assert!(has(&r.args, "-preset", qsv), "{tier:?}");
            let q = RenderQuality {
                preset: tier,
                caps: EncoderCaps::measured(Backend::Nvenc, "h264_nvenc"),
            };
            let r = render_with(&job("libx264", "23"), Backend::Nvenc, &TranslateOpts::default(), &q);
            assert!(has(&r.args, "-preset", nvenc), "{tier:?}");
        }
    }

    #[test]
    fn missing_preset_is_inserted_and_cpu_and_copies_untouched() {
        let mut a = job("libx264", "23");
        let n = a.len();
        a.splice(n - 12..n - 10, []); // drop -preset
        let r = render(&a, Backend::Qsv, &TranslateOpts::default());
        assert!(has(&r.args, "-preset", "medium"));
        // CPU keeps Jellyfin's own options (plus temp_file).
        let c = render(&job("libx264", "23"), Backend::Cpu, &TranslateOpts::default());
        assert!(has(&c.args, "-crf", "23"));
        assert!(has(&c.args, "-preset", "veryfast"));
        assert!(!c.args.iter().any(|x| x == "-b:v" || x == "-extbrc"));
        // Stream copies are not encoded: nothing to rate-control.
        let mut copy = job("libx264", "23");
        let i = copy.iter().position(|x| x == "libx264").unwrap();
        copy[i] = "copy".into();
        let r = render(&copy, Backend::Qsv, &TranslateOpts::default());
        assert!(!r.args.iter().any(|x| x == "-b:v" || x == "-preset"));
    }

    #[test]
    fn unmeasured_encoders_keep_the_spike_translation() {
        let mut a = job("libsvtav1", "30");
        let i = a.iter().position(|x| x == "-crf").unwrap();
        a[i] = "-crf".into(); // svt uses -crf too; translate maps libsvtav1 -> av1_qsv
        let r = render(&a, Backend::Qsv, &TranslateOpts::default());
        assert!(has(&r.args, "-global_quality", "30"));
        assert!(!r.args.iter().any(|x| x == "-b:v" || x == "-extbrc"));
        // ...and an encoder the backend cannot map at all.
        let mut b = job("libvpx-vp9", "31");
        let r = render(&b, Backend::Qsv, &TranslateOpts::default());
        assert!(has(&r.args, "-crf", "31"));
    }

    #[test]
    fn preset_tier_parses() {
        assert_eq!(PresetTier::parse("fastest"), Some(PresetTier::Fastest));
        assert_eq!(PresetTier::parse("calibrated"), Some(PresetTier::Calibrated));
        assert_eq!(PresetTier::parse("slowest"), Some(PresetTier::Slowest));
        assert_eq!(PresetTier::parse("turbo"), None);
    }
}
