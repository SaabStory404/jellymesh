//! The two ffmpeg command lines of a DV7 -> 8.1 remux, derived from Jellyfin's own argv
//! (`transcode/docs/PLAN.md` P5). Pure: argv in, argv out.
//!
//! - **ffmpeg#1 (demux)**: Jellyfin's input options (so `-ss`, `-t`, `-noaccurate_seek`, probe
//!   limits all apply exactly as in the plain remux) + `-map 0:v:0 -c:v copy` into MPEG-TS on
//!   stdout, with `-copyts` (source timestamps untouched), `-muxdelay 0 -muxpreload 0` (else
//!   mpegtsenc shifts everything by its mux delay) and a fixed `-output_ts_offset` of
//!   `TS_OFFSET` seconds so a first frame's DTS below its PTS can never go negative (instead of
//!   letting `-avoid_negative_ts` pick an unknown shift).
//! - **ffmpeg#2 (mux)**: input 0 = the rewritten TS on `pipe:<fd>`, input 1 = the source again
//!   with Jellyfin's input options (audio, subtitles, everything but the video), then Jellyfin's
//!   output options with the `-map`s re-pointed and `-strict unofficial` (the mp4 muxer only
//!   writes `dvcC`/`dvvC` at that compliance level -- MEASURED on ffmpeg 8.1.3, no record without
//!   it).
//!
//! **Timestamps.** ffmpeg subtracts each input's *own* start time under `-start_at_zero`, and the
//! TS input's start is the seek point while the source's is ~0, so video and audio would shift by
//! different amounts. Instead ffmpeg#2 always runs `-copyts` without `-start_at_zero`, and the
//! plain remux's offset is applied explicitly as `-itsoffset` on both inputs: `-start_time` of
//! the source under `-copyts -start_at_zero`, 0 under bare `-copyts` (and minus `TS_OFFSET` on the
//! TS input). An argv without `-copyts` is refused (the caller falls back to the plain remux):
//! Jellyfin's remux always sets it, and without it ffmpeg's seek/trim arithmetic differs in ways
//! this plan does not model.

/// Seconds added by ffmpeg#1 (`-output_ts_offset`) and removed again on ffmpeg#2's TS input.
pub const TS_OFFSET: f64 = 10.0;

/// What the plan needs to know about the source, from ffprobe.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Source {
    /// Absolute index of the video stream ffmpeg#1 maps (`-select_streams v:0`).
    pub video_index: u32,
    /// Container start time in seconds (`format=start_time`), 0 when unknown.
    pub start_time: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub demux: Vec<String>,
    pub mux: Vec<String>,
}

fn s(v: &str) -> String {
    v.to_string()
}

/// ffmpeg's time syntax: `[-][HH:]MM:SS[.frac]` or `[-]S[.frac]` (plus `s`/`ms`/`us` suffixes).
pub fn parse_time(v: &str) -> Option<f64> {
    let v = v.trim();
    let (neg, v) = match v.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, v),
    };
    let secs = if v.contains(':') {
        let parts: Vec<&str> = v.split(':').collect();
        if parts.len() > 3 {
            return None;
        }
        let mut total = 0.0;
        for p in &parts {
            total = total * 60.0 + p.parse::<f64>().ok()?;
        }
        total
    } else if let Some(n) = v.strip_suffix("ms") {
        n.parse::<f64>().ok()? / 1e3
    } else if let Some(n) = v.strip_suffix("us") {
        n.parse::<f64>().ok()? / 1e6
    } else {
        v.strip_suffix('s').unwrap_or(v).parse::<f64>().ok()?
    };
    if !secs.is_finite() {
        return None;
    }
    Some(if neg { -secs } else { secs })
}

fn fmt_secs(v: f64) -> String {
    // ffmpeg's duration parser takes plain decimal seconds, negative allowed.
    let v = if v == 0.0 { 0.0 } else { v }; // no "-0.000000"
    format!("{v:.6}")
}

/// Does this `-map` spec (without the leading `-` for a negative map) select the source video
/// stream ffmpeg#1 extracts?
fn is_video_map(spec: &str, video_index: u32) -> bool {
    let spec = spec.strip_suffix('?').unwrap_or(spec);
    let Some(rest) = spec.strip_prefix("0:") else {
        return false;
    };
    rest == video_index.to_string() || matches!(rest, "v" | "v:0" | "V" | "V:0")
}

/// Remove DV-stripping bitstream filters from a `-bsf:v` value (Jellyfin's HDR10 fallback strips
/// the RPU; ffmpeg#2 must not strip what this job just converted). `None` = drop the option.
fn without_dovi_strip(chain: &str) -> Option<String> {
    let kept: Vec<&str> = chain
        .split(',')
        .filter(|f| !(f.starts_with("dovi_rpu") || f.contains("remove_dovi")))
        .collect();
    (!kept.is_empty()).then(|| kept.join(","))
}

/// Build ffmpeg#1 and ffmpeg#2 from Jellyfin's (rendered, marker-stripped) argv. `Err` = this
/// argv shape is not supported; the caller runs the plain remux instead.
pub fn plan(args: &[String], src: &Source, fd: i32) -> Result<Plan, String> {
    let inputs: Vec<usize> = args
        .iter()
        .enumerate()
        .filter(|(_, a)| *a == "-i")
        .map(|(i, _)| i)
        .collect();
    let [i] = inputs[..] else {
        return Err(format!("expected exactly one -i, found {}", inputs.len()));
    };
    let input = args.get(i + 1).ok_or("-i without a value")?;
    let in_opts = &args[..i];
    let out_opts = &args[i + 2..];
    if out_opts.is_empty() {
        return Err("no output".into());
    }

    let has = |set: &[String], f: &str| set.iter().any(|a| a == f);
    if !has(args, "-copyts") {
        return Err("argv without -copyts is not supported".into());
    }
    for f in ["-itsoffset", "-sseof", "-output_ts_offset"] {
        if has(args, f) {
            return Err(format!("{f} is not supported"));
        }
    }
    if has(out_opts, "-ss") {
        return Err("output-side -ss is not supported".into());
    }
    let start_at_zero = has(args, "-start_at_zero");
    let ss = match in_opts.iter().position(|a| a == "-ss") {
        Some(p) => in_opts
            .get(p + 1)
            .and_then(|v| parse_time(v))
            .ok_or("unparsable -ss")?,
        None => 0.0,
    };
    let accurate_seek = ss != 0.0 && !has(in_opts, "-noaccurate_seek");
    let start_time = if src.start_time.is_finite() {
        src.start_time
    } else {
        0.0
    };
    // With -start_at_zero, the plain remux's accurate-seek trim point and its timestamp shift
    // both involve the source start time in ways -itsoffset reproduces only when it is 0.
    if start_at_zero && accurate_seek && start_time.abs() > 0.0005 {
        return Err(format!(
            "-start_at_zero + accurate seek on a source starting at {start_time}s"
        ));
    }
    let ofs = if start_at_zero { -start_time } else { 0.0 };

    let mut demux: Vec<String> = in_opts.to_vec();
    demux.extend(
        [
            "-nostdin",
            "-i",
            input,
            "-map",
            "0:v:0",
            "-c:v",
            "copy",
            "-an",
            "-sn",
            "-dn",
            "-map_metadata",
            "-1",
            "-map_chapters",
            "-1",
            "-copyts",
            "-avoid_negative_ts",
            "disabled",
            "-output_ts_offset",
        ]
        .map(s),
    );
    demux.push(fmt_secs(TS_OFFSET));
    demux.extend(
        [
            "-muxdelay",
            "0",
            "-muxpreload",
            "0",
            "-f",
            "mpegts",
            "pipe:1",
        ]
        .map(s),
    );

    let mut mux: Vec<String> = [
        "-f",
        "mpegts",
        "-probesize",
        "33554432",
        "-analyzeduration",
        "2000000",
        "-itsoffset",
    ]
    .map(s)
    .to_vec();
    mux.push(fmt_secs(ofs - TS_OFFSET));
    mux.extend([s("-i"), format!("pipe:{fd}")]);
    mux.extend_from_slice(in_opts);
    // Keep demuxing the source's video although nothing maps it: MEASURED on ffmpeg 8.1.3 with
    // the synthetic fixture, `-ss 1.5 -noaccurate_seek` on an MKV whose video stream is unused
    // started the audio at 1.003 s instead of the plain remux's 0.811 s (ffmpeg discards unused
    // streams, which changes where the matroska seek lands). `-discard:v:0 none` restores the
    // plain remux's packet flow; the packets are dropped unused.
    mux.extend([s("-discard:v:0"), s("none")]);
    mux.extend([s("-itsoffset"), fmt_secs(ofs), s("-i"), input.clone()]);

    let (out_body, out_path) = out_opts.split_at(out_opts.len() - 1);
    let mut saw_video_map = false;
    let mut saw_tag = false;
    let mut k = 0;
    while k < out_body.len() {
        let a = &out_body[k];
        let v = out_body.get(k + 1);
        match (a.as_str(), v) {
            ("-map", Some(v)) => {
                let (neg, spec) = match v.strip_prefix('-') {
                    Some(r) => (true, r),
                    None => (false, v.as_str()),
                };
                if is_video_map(spec, src.video_index) {
                    if !neg {
                        saw_video_map = true;
                        mux.extend([s("-map"), s("0:v:0")]);
                    }
                } else if let Some(rest) = spec.strip_prefix("0:") {
                    let neg = if neg { "-" } else { "" };
                    mux.extend([s("-map"), format!("{neg}1:{rest}")]);
                } else {
                    return Err(format!("unsupported -map {v}"));
                }
                k += 2;
            }
            ("-map_metadata" | "-map_chapters", Some(v)) => {
                let v = if v == "0" { "1" } else { v.as_str() };
                mux.extend([a.clone(), s(v)]);
                k += 2;
            }
            ("-copyts" | "-start_at_zero", _) => k += 1,
            ("-strict", Some(v)) => {
                // Keep a level that already allows unofficial boxes; else raise it.
                let lvl = if matches!(v.as_str(), "-1" | "-2" | "unofficial" | "experimental") {
                    v.clone()
                } else {
                    s("unofficial")
                };
                mux.extend([s("-strict"), lvl]);
                k += 2;
            }
            (b, Some(v)) if b.starts_with("-bsf:v") || b == "-bsf" => {
                if let Some(chain) = without_dovi_strip(v) {
                    mux.extend([a.clone(), chain]);
                }
                k += 2;
            }
            _ => {
                if a.starts_with("-tag:v") {
                    saw_tag = true;
                }
                mux.push(a.clone());
                k += 1;
            }
        }
    }
    if !saw_video_map {
        return Err("no -map selects the source video stream".into());
    }
    if !mux.iter().any(|a| a == "-strict") {
        mux.extend([s("-strict"), s("unofficial")]);
    }
    if !saw_tag {
        mux.extend([s("-tag:v:0"), s("hvc1")]);
    }
    mux.push(s("-copyts"));
    mux.extend_from_slice(out_path);
    Ok(Plan { demux, mux })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// The shape of Jellyfin's HLS fmp4 remux (EncodingHelper / DynamicHlsController for a video
    /// stream copy), with the P5 marker already stripped.
    fn remux(ss: Option<&str>) -> Vec<String> {
        let mut v = a(&["-analyzeduration", "200M", "-probesize", "50M"]);
        if let Some(t) = ss {
            v.extend(a(&["-ss", t, "-noaccurate_seek"]));
        }
        v.extend(a(&[
            "-f",
            "matroska,webm",
            "-i",
            "file:/data/media/movies/X/X.mkv",
            "-map_metadata",
            "-1",
            "-map_chapters",
            "-1",
            "-threads",
            "0",
            "-map",
            "0:0",
            "-map",
            "0:1",
            "-map",
            "-0:s",
            "-codec:v:0",
            "copy",
            "-tag:v:0",
            "dvh1",
            "-strict",
            "-2",
            "-bsf:v",
            "hevc_metadata=remove_dovi=1",
            "-start_at_zero",
            "-codec:a:0",
            "libfdk_aac",
            "-ac",
            "2",
            "-copyts",
            "-avoid_negative_ts",
            "disabled",
            "-max_muxing_queue_size",
            "2048",
            "-f",
            "hls",
            "-max_delay",
            "5000000",
            "-hls_time",
            "6",
            "-hls_segment_type",
            "fmp4",
            "-hls_fmp4_init_filename",
            "abc-1.mp4",
            "-start_number",
            "0",
            "-hls_segment_filename",
            "/transcodes/jf/abc%d.mp4",
            "-hls_playlist_type",
            "vod",
            "-hls_list_size",
            "0",
            "-hls_flags",
            "temp_file",
            "-y",
            "/transcodes/jf/abc.m3u8",
        ]));
        v
    }

    /// The Jellyfin-side decision patch's actual argv shape (jellymesh PR #3, bug-hunt patch 13):
    /// a TS-sourced HLS fmp4 remux with `-bsf:v hevc_mp4toannexb` already present in the output
    /// options (unrelated to the DV-strip bsfs `without_dovi_strip` targets) and the P5 marker
    /// pair alongside it. `video_index` need not be 0.
    fn remux_annexb_bsf() -> Vec<String> {
        a(&[
            "-analyzeduration",
            "200M",
            "-probesize",
            "50M",
            "-copyts",
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
            "-map",
            "-0:s",
            "-codec:v:0",
            "copy",
            "-bsf:v",
            "hevc_mp4toannexb",
            "-tag:v:0",
            "hvc1",
            "-codec:a:0",
            "libfdk_aac",
            "-ac",
            "2",
            "-avoid_negative_ts",
            "disabled",
            "-max_muxing_queue_size",
            "2048",
            "-f",
            "hls",
            "-hls_segment_type",
            "fmp4",
            "-hls_fmp4_init_filename",
            "abc-1.mp4",
            "-start_number",
            "0",
            "-hls_segment_filename",
            "/transcodes/jf/abc%d.mp4",
            "-hls_playlist_type",
            "vod",
            "-hls_list_size",
            "0",
            "-hls_flags",
            "temp_file",
            "-y",
            "/transcodes/jf/abc.m3u8",
        ])
    }

    const SRC: Source = Source {
        video_index: 0,
        start_time: 0.0,
    };

    fn pos(v: &[String], x: &str) -> usize {
        v.iter()
            .position(|a| a == x)
            .unwrap_or_else(|| panic!("{x} missing"))
    }

    #[test]
    fn parses_ffmpeg_times() {
        assert_eq!(parse_time("00:05:00.500"), Some(300.5));
        assert_eq!(parse_time("05:00"), Some(300.0));
        assert_eq!(parse_time("12.25"), Some(12.25));
        assert_eq!(parse_time("-1.5"), Some(-1.5));
        assert_eq!(parse_time("1500ms"), Some(1.5));
        assert_eq!(parse_time("nope"), None);
    }

    #[test]
    fn demux_keeps_jellyfins_input_options_and_extracts_video_to_ts() {
        let p = plan(&remux(Some("00:10:00.000")), &SRC, 3).unwrap();
        let d = &p.demux;
        let i = pos(d, "-i");
        assert_eq!(d[i + 1], "file:/data/media/movies/X/X.mkv");
        assert!(d[..i].windows(2).any(|w| w == ["-ss", "00:10:00.000"]));
        assert!(d[..i].iter().any(|x| x == "-noaccurate_seek"));
        for w in [
            ["-map", "0:v:0"],
            ["-c:v", "copy"],
            ["-output_ts_offset", "10.000000"],
            ["-muxdelay", "0"],
            ["-muxpreload", "0"],
            ["-f", "mpegts"],
        ] {
            assert!(d.windows(2).any(|x| x == w), "{w:?} missing from {d:?}");
        }
        assert!(d.iter().any(|x| x == "-copyts"));
        assert_eq!(d.last().unwrap(), "pipe:1");
    }

    #[test]
    fn mux_reads_ts_on_the_fd_and_the_source_for_everything_else() {
        let p = plan(&remux(Some("600")), &SRC, 3).unwrap();
        let m = &p.mux;
        let inputs: Vec<usize> = m
            .iter()
            .enumerate()
            .filter(|(_, x)| *x == "-i")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(inputs.len(), 2);
        assert_eq!(m[inputs[0] + 1], "pipe:3");
        assert_eq!(m[inputs[0] - 1], "-10.000000", "TS input undoes the +10s");
        assert_eq!(m[inputs[1] + 1], "file:/data/media/movies/X/X.mkv");
        assert_eq!(m[inputs[1] - 1], "0.000000");
        // Jellyfin's -ss applies to the source input (between the two -i's), not the TS.
        assert!(m[inputs[0]..inputs[1]]
            .windows(2)
            .any(|w| w == ["-ss", "600"]));
        assert!(!m[..inputs[0]].iter().any(|x| x == "-ss"));
    }

    #[test]
    fn mux_repoints_maps_and_drops_the_dovi_strip() {
        let p = plan(&remux(None), &SRC, 3).unwrap();
        let m = &p.mux;
        let maps: Vec<&str> = m
            .windows(2)
            .filter(|w| w[0] == "-map")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(maps, ["0:v:0", "1:1", "-1:s"]);
        assert!(!m.iter().any(|x| x.contains("remove_dovi")));
        assert!(!m.iter().any(|x| x == "-start_at_zero"));
        assert_eq!(m.iter().filter(|x| *x == "-copyts").count(), 1);
        assert!(m.windows(2).any(|w| w == ["-strict", "-2"]));
        assert!(m.windows(2).any(|w| w == ["-tag:v:0", "dvh1"]));
        assert!(m.windows(2).any(|w| w == ["-map_metadata", "-1"]));
        assert_eq!(m.last().unwrap(), "/transcodes/jf/abc.m3u8");
        assert!(m.windows(2).any(|w| w == ["-hls_segment_type", "fmp4"]));
    }

    #[test]
    fn mux_adds_strict_and_tag_when_absent() {
        let mut args = remux(None);
        for f in ["-strict", "-tag:v:0"] {
            let i = pos(&args, f);
            args.drain(i..i + 2);
        }
        let m = plan(&args, &SRC, 3).unwrap().mux;
        assert!(m.windows(2).any(|w| w == ["-strict", "unofficial"]));
        assert!(m.windows(2).any(|w| w == ["-tag:v:0", "hvc1"]));
    }

    #[test]
    fn start_at_zero_offsets_both_inputs_by_the_source_start() {
        let src = Source {
            video_index: 0,
            start_time: 1.5,
        };
        let m = plan(&remux(Some("60")), &src, 3).unwrap().mux;
        let offs: Vec<&str> = m
            .windows(2)
            .filter(|w| w[0] == "-itsoffset")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(offs, ["-11.500000", "-1.500000"]);
    }

    #[test]
    fn unsupported_shapes_are_refused_not_guessed() {
        let mut no_copyts = remux(None);
        no_copyts.retain(|x| x != "-copyts");
        assert!(plan(&no_copyts, &SRC, 3).is_err());

        let mut two_inputs = remux(None);
        let i = pos(&two_inputs, "-i");
        two_inputs.splice(i..i, a(&["-i", "/x/ext.ac3"]));
        assert!(plan(&two_inputs, &SRC, 3).is_err());

        let mut accurate = remux(None);
        let i = pos(&accurate, "-f");
        accurate.splice(i..i, a(&["-ss", "60"]));
        let src = Source {
            video_index: 0,
            start_time: 2.0,
        };
        assert!(plan(&accurate, &src, 3).is_err());

        let mut other_video = remux(None);
        let i = pos(&other_video, "0:0");
        other_video[i] = "0:5".into();
        assert!(plan(&other_video, &SRC, 3).is_err(), "no map selects video");
    }

    #[test]
    fn mux_keeps_hevc_mp4toannexb_bsf_untouched_and_repoints_a_nonzero_video_map() {
        // `-bsf:v hevc_mp4toannexb` is not a DV-stripping filter (`without_dovi_strip` only
        // targets `dovi_rpu*`/`remove_dovi`), so it must survive into ffmpeg#2's argv exactly as
        // Jellyfin wrote it -- MEASURED 2026-09-27 on ffmpeg 8.1.3: applying it to an
        // already-Annex-B HEVC stream (as ffmpeg#2's TS input always is) copied into an mp4/fmp4
        // output produced a byte-identical file to the same copy without the bsf, so keeping it in
        // the mux argv is harmless.
        let src = Source {
            video_index: 2,
            start_time: 0.0,
        };
        let p = plan(&remux_annexb_bsf(), &src, 3).unwrap();
        let m = &p.mux;
        assert!(
            m.windows(2).any(|w| w == ["-bsf:v", "hevc_mp4toannexb"]),
            "hevc_mp4toannexb must survive into the mux argv: {m:?}"
        );
        let maps: Vec<&str> = m
            .windows(2)
            .filter(|w| w[0] == "-map")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(maps, ["0:v:0", "1:1", "-1:s"]);
        assert!(m.windows(2).any(|w| w == ["-tag:v:0", "hvc1"]));
        assert_eq!(m.last().unwrap(), "/transcodes/jf/abc.m3u8");
        assert!(m.windows(2).any(|w| w == ["-hls_segment_type", "fmp4"]));
    }

    #[test]
    fn video_map_matches_the_probed_index() {
        assert!(is_video_map("0:2", 2));
        assert!(is_video_map("0:v:0?", 7));
        assert!(!is_video_map("0:1", 2));
        assert!(!is_video_map("1:0", 0));
    }
}
