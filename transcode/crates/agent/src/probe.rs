//! Startup capability probe: what this card can really output, measured by test encodes.
//!
//! Measured, not declared: e.g. the Tesla P4 lists `av1_nvenc` in `ffmpeg -encoders` but cannot
//! encode AV1. The GPU tonemap probe also warms the CUDA JIT cache: without it the P4 spent ~12 s
//! compiling kernels before the first frame of every job (MEASURED).

use crate::config::Config;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tcpool_ir::{translate, Backend, TranslateOpts};
use tokio::process::Command;

/// Output tokens -> (Jellyfin software encoder, 10-bit).
pub const OUTPUTS: [(&str, &str, bool); 5] = [
    ("h264", "libx264", false),
    ("hevc", "libx265", false),
    ("hevc10", "libx265", true),
    ("av1", "libsvtav1", false),
    ("av1-10", "libsvtav1", true),
];

pub const HDR_VF: &str = r"setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc,scale=trunc(min(max(iw\,ih*a)\,1920)/2)*2:trunc(ow/a/2)*2,tonemapx=tonemap=bt2390:desat=0:peak=100:t=bt709:m=bt709:p=bt709:format=yuv420p";

#[derive(Debug, Clone, Default)]
pub struct Probed {
    pub outputs: Vec<String>,
    pub gpu_tonemap: bool,
    pub ffmpeg_version: String,
}

async fn run_quiet(prog: &str, args: &[String], timeout: Duration) -> Result<(), String> {
    let child = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Err(_) => Err(format!("timed out after {timeout:?}")),
        Ok(Err(e)) => Err(e.to_string()),
        Ok(Ok(o)) if o.status.success() => Ok(()),
        Ok(Ok(o)) => {
            let err = String::from_utf8_lossy(&o.stderr);
            Err(err
                .lines()
                .last()
                .unwrap_or("failed")
                .chars()
                .take(150)
                .collect())
        }
    }
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

pub async fn probe(cfg: &Config) -> Probed {
    let init: Vec<String> = cfg
        .backend
        .device_init_args()
        .iter()
        .map(|x| x.to_string())
        .collect();
    let mut outputs = Vec::new();
    for (token, sw, ten) in OUTPUTS {
        let Some(enc) = cfg.backend.encoder_for(sw) else {
            continue;
        };
        let fmt = match (cfg.backend, ten) {
            (Backend::Cpu, true) => "yuv420p10le",
            (Backend::Cpu, false) => "yuv420p",
            (_, true) => "p010le",
            (_, false) => "nv12",
        };
        let mut args = s(&["-hide_banner", "-v", "error"]);
        args.extend(init.iter().cloned());
        args.extend(s(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=320x240:d=0.5:r=24",
            "-vf",
            &format!("format={fmt}"),
            "-c:v",
            enc,
        ]));
        if ten && token.starts_with("hevc") && cfg.backend != Backend::Cpu {
            args.extend(s(&["-profile:v", "main10"]));
        }
        args.extend(s(&["-f", "null", "-"]));
        match run_quiet(&cfg.ffmpeg, &args, Duration::from_secs(30)).await {
            Ok(()) => {
                crate::log(format_args!("probe {token} ({enc}): ok"));
                outputs.push(token.to_string());
            }
            Err(e) => crate::log(format_args!("probe {token} ({enc}): NO {e}")),
        }
    }
    if let Some(allow) = &cfg.outputs_allow {
        outputs.retain(|o| allow.contains(o));
    }
    let gpu_tonemap = probe_gpu_tonemap(cfg, &init).await;
    let ffmpeg_version = ffmpeg_version(&cfg.ffmpeg).await;
    Probed {
        outputs,
        gpu_tonemap,
        ffmpeg_version,
    }
}

async fn probe_gpu_tonemap(cfg: &Config, init: &[String]) -> bool {
    if cfg.backend == Backend::Cpu || !cfg.gpu_filters {
        return false;
    }
    let started = Instant::now();
    let clip = std::env::temp_dir()
        .join(format!("tcpool-probe-hdr-{}.mkv", cfg.name))
        .to_string_lossy()
        .into_owned();
    let Some(enc) = cfg.backend.encoder_for("libx265") else {
        return false;
    };
    let mut make = s(&["-hide_banner", "-v", "error", "-y"]);
    make.extend(init.iter().cloned());
    make.extend(s(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=s=1280x720:d=1:r=24",
        "-vf",
        "format=p010le",
        "-c:v",
        enc,
        "-profile:v",
        "main10",
        "-color_primaries",
        "bt2020",
        "-color_trc",
        "smpte2084",
        "-colorspace",
        "bt2020nc",
        &clip,
    ]));
    let job = s(&[
        "-i",
        &clip,
        "-codec:v:0",
        "libx264",
        "-preset",
        "veryfast",
        "-vf",
        HDR_VF,
        "-f",
        "null",
        "-",
    ]);
    let t = translate(
        &job,
        cfg.backend,
        &TranslateOpts {
            pathmap: vec![],
            gpu_filters: true,
            ..Default::default()
        },
    );
    let mut run = s(&["-hide_banner", "-v", "error"]);
    run.extend(t.args);
    let res = match run_quiet(&cfg.ffmpeg, &make, Duration::from_secs(60)).await {
        Ok(()) if t.gpu_filters => run_quiet(&cfg.ffmpeg, &run, Duration::from_secs(120)).await,
        Ok(()) => Err("chain not translatable".into()),
        Err(e) => Err(format!("clip: {e}")),
    };
    let _ = std::fs::remove_file(&clip);
    let secs = started.elapsed().as_secs_f64();
    match res {
        Ok(()) => {
            crate::log(format_args!("probe gpu tonemap: ok ({secs:.1}s)"));
            true
        }
        Err(e) => {
            crate::log(format_args!("probe gpu tonemap: NO {e} ({secs:.1}s)"));
            false
        }
    }
}

async fn ffmpeg_version(ffmpeg: &str) -> String {
    match Command::new(ffmpeg).arg("-version").output().await {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(2))
            .unwrap_or("")
            .to_string(),
        Err(_) => String::new(),
    }
}

/// Height of the job's video input via ffprobe (bounded). `None` if unknown.
pub async fn source_height(cfg: &Config, input: &str) -> Option<u32> {
    let out = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new(&cfg.ffprobe)
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=height",
                "-of",
                "csv=p=0",
                input,
            ])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}

/// What the P5 gate needs to know about a source's video track.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SourceDovi {
    /// DOVI configuration record profile (7, 8, 5, ...), `None` for a non-DV track.
    pub profile: Option<u8>,
    pub level: u8,
    /// Absolute stream index of the first video stream.
    pub video_index: u32,
    /// Container start time in seconds (0 when ffprobe reports none).
    pub start_time: f64,
}

/// The first video track's Dolby Vision configuration record, index and the container start
/// time, via ffprobe; `None` if ffprobe fails, times out, or finds no video stream.
///
/// This is the source-truth half of the P5 gate (`transcode/docs/PLAN.md`): `tcpool_ir::wants_dv81`
/// says what Jellyfin's decision patch asked for; this says what the file actually is. `job.rs`
/// requires `profile == Some(7)` before converting; anything else (including `None` from a
/// timeout) falls through to the plain remux -- fail closed, never convert an unconfirmed source.
///
/// Uses `stream_side_data=`, not `side_data=`: MEASURED 2026-09-27 on ffmpeg 8.1.3, the
/// unqualified `side_data` section also selects packet and frame side data, so
/// `-show_entries stream=index:side_data=dv_profile` read all 96 packets of a 4 s test clip --
/// on a 70 GB remux it would hit the 10 s timeout and fail closed on every job. The qualified
/// form reads headers only.
pub async fn source_dovi(cfg: &Config, input: &str) -> Option<SourceDovi> {
    let out = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new(&cfg.ffprobe)
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=index:stream_side_data=dv_profile,dv_level:format=start_time",
                "-of",
                "json",
                input,
            ])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_source_dovi_json(&String::from_utf8_lossy(&out.stdout))
}

/// Pure parse of `source_dovi`'s ffprobe JSON (MEASURED shape, ffmpeg 8.1.3:
/// `{"streams":[{"index":0,"side_data_list":[{"dv_profile":8,"dv_level":6}]}],
/// "format":{"start_time":"0.083000"}}`; the same `side_data_list`/`dv_profile` nesting was
/// MEASURED on jellyfin-ffmpeg 8.1.2 in `tc-lab` against a real profile-7 source).
fn parse_source_dovi_json(json: &str) -> Option<SourceDovi> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let stream = v.get("streams")?.as_array()?.first()?;
    let video_index = u32::try_from(stream.get("index")?.as_u64()?).ok()?;
    let dv = stream
        .get("side_data_list")
        .and_then(|l| l.as_array())
        .and_then(|l| l.iter().find(|sd| sd.get("dv_profile").is_some()));
    let profile = dv
        .and_then(|sd| sd.get("dv_profile")?.as_u64())
        .and_then(|p| u8::try_from(p).ok());
    let level = dv
        .and_then(|sd| sd.get("dv_level")?.as_u64())
        .and_then(|l| u8::try_from(l).ok())
        .unwrap_or(0);
    let start_time = v
        .get("format")
        .and_then(|f| f.get("start_time"))
        .and_then(|s| s.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|s| s.is_finite())
        .unwrap_or(0.0);
    Some(SourceDovi {
        profile,
        level,
        video_index,
        start_time,
    })
}

#[cfg(test)]
mod dv81_tests {
    use super::*;

    #[test]
    fn parses_the_measured_shape() {
        let json = r#"{
            "programs": [],
            "streams": [
                {"index": 0, "side_data_list": [{"dv_profile": 7, "dv_level": 6}]}
            ],
            "format": {"start_time": "0.083000"}
        }"#;
        assert_eq!(
            parse_source_dovi_json(json),
            Some(SourceDovi {
                profile: Some(7),
                level: 6,
                video_index: 0,
                start_time: 0.083
            })
        );
    }

    #[test]
    fn a_non_dv_video_track_has_no_profile() {
        let json = r#"{"streams": [{"index": 2}], "format": {}}"#;
        let s = parse_source_dovi_json(json).unwrap();
        assert_eq!(s.profile, None);
        assert_eq!(s.video_index, 2);
        assert_eq!(s.start_time, 0.0);
    }

    #[test]
    fn garbage_or_no_video_is_none() {
        assert_eq!(parse_source_dovi_json("not json"), None);
        assert_eq!(parse_source_dovi_json("{}"), None);
        assert_eq!(parse_source_dovi_json(r#"{"streams": []}"#), None);
    }

    #[test]
    fn a_profile_that_does_not_fit_a_u8_is_not_a_profile() {
        let json = r#"{"streams":[{"index":0,"side_data_list":[{"dv_profile": 99999}]}]}"#;
        assert_eq!(parse_source_dovi_json(json).unwrap().profile, None);
    }
}
