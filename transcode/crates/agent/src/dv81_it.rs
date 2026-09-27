//! End-to-end test of the P5 DV7 -> 8.1 job path against a synthetic profile-7 fixture, through
//! the real `run_dv81` / `run_ffmpeg` (fd-3 pipe, fallback, metrics) and the local `ffmpeg`.
//!
//! The fixture: x265 10-bit video + AAC audio, with a synthetic profile-7 MEL RPU (NAL 62) and a
//! fake enhancement-layer NAL (63) appended to every access unit, muxed to MKV with a profile-7
//! DOVI configuration record -- built with this crate's own `TsRewriter` (inject mode) and ffmpeg
//! stream copies, so it needs no binary test asset. Nothing here is derived from a real title.
//!
//! Skips (passes with a note on stderr) when `ffmpeg`/`ffprobe` or libx265 are missing, so CI
//! without ffmpeg stays green; run locally for the real check.

use super::*;
use crate::dv81::testutil::synthetic_p7_rpu_nal;
use crate::dv81::{split_annexb, EL_NAL_UNIT_TYPE, RPU_NAL_UNIT_TYPE};
use crate::dv81_ts::{DoviDescriptor, TransformOut, TsRewriter};
use dolby_vision::rpu::dovi_rpu::DoviRpu;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

fn have_tools() -> bool {
    let enc = StdCommand::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output();
    let probe = StdCommand::new("ffprobe").arg("-version").output();
    matches!((enc, probe), (Ok(e), Ok(p))
        if p.status.success() && String::from_utf8_lossy(&e.stdout).contains("libx265"))
}

fn tmpdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("dv81-it-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ffmpeg(args: &[&str]) {
    let out = StdCommand::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .expect("spawn ffmpeg");
    assert!(
        out.status.success(),
        "ffmpeg {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn p(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// base.ts (x265 + AAC, 96 frames at 23.976) -> inj.ts (RPU + EL per AU, P7 descriptor) ->
/// fixture.mkv. With `inject == false` the video gets the P7 descriptor but no in-band RPU: the
/// shape of an `hvcE` Block Addition source as ffmpeg sees it.
fn build_fixture(dir: &Path, inject: bool) -> PathBuf {
    let base = dir.join("base.ts");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=s=256x144:r=24000/1001:d=4",
        "-f",
        "lavfi",
        "-i",
        "sine=f=440:r=48000:d=4",
        "-map",
        "0:v",
        "-map",
        "1:a",
        "-c:v",
        "libx265",
        "-pix_fmt",
        "yuv420p10le",
        "-x265-params",
        "log-level=error:bframes=3:keyint=24:min-keyint=24:scenecut=0",
        "-color_primaries",
        "bt2020",
        "-color_trc",
        "smpte2084",
        "-colorspace",
        "bt2020nc",
        "-c:a",
        "aac",
        "-muxdelay",
        "0",
        "-muxpreload",
        "0",
        "-f",
        "mpegts",
        p(&base),
    ]);
    let rpu = synthetic_p7_rpu_nal();
    let inject_fn = move |au: &[u8]| -> Result<TransformOut, String> {
        let mut v = au.to_vec();
        if inject {
            v.extend_from_slice(&[0, 0, 0, 1]);
            v.extend_from_slice(&crate::dv81::testutil::nal_bytes(
                EL_NAL_UNIT_TYPE,
                0,
                b"fake-enhancement-layer",
            ));
            v.extend_from_slice(&[0, 0, 0, 1]);
            v.extend_from_slice(&rpu);
        }
        Ok((v, 0, 0))
    };
    let mut rw = TsRewriter::new(DoviDescriptor::profile_7(6), inject_fn);
    let mut out = Vec::new();
    rw.push(&std::fs::read(&base).unwrap(), &mut out).unwrap();
    rw.finish(&mut out).unwrap();
    let inj = dir.join("inj.ts");
    std::fs::write(&inj, out).unwrap();
    let mkv = dir.join("fixture.mkv");
    ffmpeg(&[
        "-i",
        p(&inj),
        "-map",
        "0",
        "-c",
        "copy",
        "-strict",
        "unofficial",
        p(&mkv),
    ]);
    mkv
}

/// Jellyfin's HLS fmp4 remux shape (video copy, audio transcode, -copyts -start_at_zero).
fn remux_args(input: &Path, out: &Path, ss: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    if let Some(t) = ss {
        a.extend(["-ss", t, "-noaccurate_seek"].map(String::from));
    }
    let seg = out.join("seg%d.mp4");
    let pl = out.join("main.m3u8");
    a.extend(
        [
            "-f",
            "matroska,webm",
            "-i",
            &format!("file:{}", p(input)),
            "-map_metadata",
            "-1",
            "-map_chapters",
            "-1",
            "-map",
            "0:0",
            "-map",
            "0:1",
            "-map",
            "-0:s",
            "-codec:v:0",
            "copy",
            "-tag:v:0",
            "hvc1",
            "-start_at_zero",
            "-codec:a:0",
            "aac",
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
            "1",
            "-hls_segment_type",
            "fmp4",
            "-hls_fmp4_init_filename",
            "init.mp4",
            "-start_number",
            "0",
            "-hls_segment_filename",
            p(&seg),
            "-hls_playlist_type",
            "vod",
            "-hls_list_size",
            "0",
            "-y",
            p(&pl),
        ]
        .map(String::from),
    );
    a
}

struct Harness {
    state: Arc<State>,
    ctl: Arc<Ctl>,
    tx: Tx,
    stderr: Arc<std::sync::Mutex<String>>,
    kill_rx: watch::Receiver<bool>,
    stdin_rx: Arc<Mutex<mpsc::Receiver<Option<Vec<u8>>>>>,
    _stdin_tx: mpsc::Sender<Option<Vec<u8>>>,
}

fn harness() -> Harness {
    let (_drain_tx, drain_rx) = watch::channel(false);
    let state = Arc::new(State {
        cfg: Config::minimal(),
        probed: crate::probe::Probed {
            outputs: vec![],
            gpu_tonemap: false,
            ffmpeg_version: String::new(),
        },
        usage: crate::Usage::new(10.0, 0.0),
        probed_unix: 0,
        drain: drain_rx,
        metrics: crate::metrics::Metrics::default(),
    });
    let (kill_tx, kill_rx) = watch::channel(false);
    let ctl = Arc::new(Ctl {
        fenced: AtomicBool::new(false),
        done: AtomicBool::new(false),
        last_heard: Mutex::new(Instant::now()),
        kill: kill_tx,
        paused: AtomicBool::new(false),
        quitting: AtomicBool::new(false),
        progress: std::sync::Mutex::new((Instant::now(), String::new(), false)),
        ended: std::sync::Mutex::new(None),
        state: state.clone(),
        job_id: 1,
    });
    let (tx, mut rx) = mpsc::channel::<Result<ServerMsg, Status>>(1024);
    let stderr = Arc::new(std::sync::Mutex::new(String::new()));
    let sink = stderr.clone();
    tokio::spawn(async move {
        while let Some(Ok(m)) = rx.recv().await {
            if let Some(server_msg::Msg::Stderr(b)) = m.msg {
                sink.lock().unwrap().push_str(&String::from_utf8_lossy(&b));
            }
        }
    });
    let (stdin_tx, stdin_rx) = mpsc::channel(8);
    Harness {
        state,
        ctl,
        tx,
        stderr,
        kill_rx,
        stdin_rx: Arc::new(Mutex::new(stdin_rx)),
        _stdin_tx: stdin_tx,
    }
}

fn dv81_count(h: &Harness, o: crate::metrics::Dv81Outcome) -> u64 {
    let text = crate::metrics::render(&h.state);
    let needle = format!("outcome=\"{}\"}} ", o.as_str());
    text.lines()
        .find(|l| l.starts_with("tcpool_dv81_total") && l.contains(&needle))
        .and_then(|l| l.rsplit(' ').next()?.parse().ok())
        .unwrap()
}

fn ffprobe(args: &[&str]) -> String {
    let out = StdCommand::new("ffprobe")
        .args(["-hide_banner"])
        .args(args)
        .output()
        .expect("spawn ffprobe");
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// init.mp4 + every segN.mp4 concatenated: one playable fragmented mp4 of the whole output.
fn concat_hls(dir: &Path) -> PathBuf {
    let mut all = std::fs::read(dir.join("init.mp4")).unwrap();
    let mut n = 0;
    while let Ok(seg) = std::fs::read(dir.join(format!("seg{n}.mp4"))) {
        all.extend(seg);
        n += 1;
    }
    assert!(n > 0, "no segments in {}", dir.display());
    let out = dir.join("all.mp4");
    std::fs::write(&out, all).unwrap();
    out
}

/// Every NAL of the output's video, as Annex-B.
fn output_nals(dir: &Path) -> Vec<crate::dv81::Nal> {
    let all = concat_hls(dir);
    let hevc = dir.join("out.hevc");
    ffmpeg(&[
        "-i",
        p(&all),
        "-map",
        "0:v:0",
        "-c",
        "copy",
        "-bsf:v",
        "hevc_mp4toannexb",
        "-f",
        "hevc",
        p(&hevc),
    ]);
    split_annexb(&std::fs::read(&hevc).unwrap())
}

/// Min PTS (seconds) per stream index of the first segment (init + seg0).
fn first_segment_start(dir: &Path) -> (f64, f64) {
    let mut first = std::fs::read(dir.join("init.mp4")).unwrap();
    first.extend(std::fs::read(dir.join("seg0.mp4")).unwrap());
    let f = dir.join("first.mp4");
    std::fs::write(&f, first).unwrap();
    let csv = ffprobe(&[
        "-v",
        "error",
        "-show_entries",
        "packet=stream_index,pts_time",
        "-of",
        "csv=p=0",
        p(&f),
    ]);
    let mut v = f64::INFINITY;
    let mut a = f64::INFINITY;
    for line in csv.lines() {
        let mut it = line.split(',');
        let (Some(idx), Some(t)) = (it.next(), it.next()) else {
            continue;
        };
        let Ok(t) = t.parse::<f64>() else { continue };
        match idx {
            "0" => v = v.min(t),
            "1" => a = a.min(t),
            _ => {}
        }
    }
    (v, a)
}

async fn run_plain(h: &Harness, args: &[String]) -> i32 {
    run_ffmpeg(
        &h.state.cfg,
        args,
        "",
        &h.tx,
        &h.ctl,
        h.kill_rx.clone(),
        h.stdin_rx.clone(),
        None,
    )
    .await
}

async fn run_signaled(h: &Harness, args: &[String]) -> i32 {
    run_dv81(
        &h.state.cfg,
        &h.state,
        args,
        "",
        &h.tx,
        &h.ctl,
        h.kill_rx.clone(),
        h.stdin_rx.clone(),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn converts_a_synthetic_dv7_source_end_to_end() {
    if !have_tools() {
        eprintln!(
            "SKIP converts_a_synthetic_dv7_source_end_to_end: ffmpeg/ffprobe/libx265 missing"
        );
        return;
    }
    let dir = tmpdir("conv");
    let src = build_fixture(&dir, true);

    // The gate's ffprobe sees the fixture as DV profile 7.
    let dovi = crate::probe::source_dovi(&Config::minimal(), p(&src))
        .await
        .expect("probe");
    assert_eq!(dovi.profile, Some(7), "fixture must probe as profile 7");

    for (tag, ss) in [("noseek", None), ("seek", Some("1.5"))] {
        let h = harness();
        let out_dv = dir.join(format!("dv-{tag}"));
        let out_plain = dir.join(format!("plain-{tag}"));
        std::fs::create_dir_all(&out_dv).unwrap();
        std::fs::create_dir_all(&out_plain).unwrap();

        let code = run_signaled(&h, &remux_args(&src, &out_dv, ss)).await;
        let stderr = h.stderr.lock().unwrap().clone();
        assert_eq!(code, 0, "[{tag}] converting pipeline failed:\n{stderr}");
        assert_eq!(dv81_count(&h, crate::metrics::Dv81Outcome::Converted), 1);
        assert!(
            !stderr.contains("Generating one"),
            "[{tag}] ffmpeg fabricated a DV config record:\n{stderr}"
        );

        // Init segment: DOVI configuration record profile 8, BL compat 1, no EL.
        let init = ffprobe(&[p(&out_dv.join("init.mp4"))]);
        assert!(
            init.contains("profile: 8")
                && init.contains("el flag: 0")
                && init.contains("compatibility id: 1"),
            "[{tag}] init segment DOVI record wrong:\n{init}"
        );

        // Every frame carries exactly one RPU, now profile 8; no EL NAL survives.
        let nals = output_nals(&out_dv);
        let rpus: Vec<_> = nals
            .iter()
            .filter(|n| n.nal_unit_type == RPU_NAL_UNIT_TYPE)
            .collect();
        let frames = nals
            .iter()
            .filter(|n| n.nal_unit_type < 32 && n.data.get(2).is_some_and(|b| b & 0x80 != 0))
            .count();
        assert!(frames > 0);
        assert_eq!(rpus.len(), frames, "[{tag}] one RPU per frame");
        assert!(!nals
            .iter()
            .any(|n| n.nal_unit_type == EL_NAL_UNIT_TYPE || n.layer_id != 0));
        for r in &rpus {
            let rpu = DoviRpu::parse_unspec62_nalu(&r.data[2..]).expect("RPU parses");
            assert_eq!(rpu.dovi_profile, 8);
        }

        // A/V timing equals the plain remux of the same argv.
        let code = run_plain(&h, &remux_args(&src, &out_plain, ss)).await;
        assert_eq!(code, 0);
        let plain_nals = output_nals(&out_plain);
        let plain_frames = plain_nals
            .iter()
            .filter(|n| n.nal_unit_type < 32 && n.data.get(2).is_some_and(|b| b & 0x80 != 0))
            .count();
        assert_eq!(
            frames, plain_frames,
            "[{tag}] same frames as the plain remux"
        );
        let (dv_v, dv_a) = first_segment_start(&out_dv);
        let (pl_v, pl_a) = first_segment_start(&out_plain);
        eprintln!("[{tag}] first segment start: dv81 video {dv_v} audio {dv_a}; plain video {pl_v} audio {pl_a}; frames {frames}");
        let frame = 1001.0 / 24000.0;
        assert!(
            (dv_v - pl_v).abs() < frame,
            "[{tag}] video start {dv_v} vs {pl_v}"
        );
        assert!(
            (dv_a - pl_a).abs() < frame,
            "[{tag}] audio start {dv_a} vs {pl_a}"
        );
        assert!(
            ((dv_a - dv_v) - (pl_a - pl_v)).abs() < frame,
            "[{tag}] A/V offset differs from the plain remux"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dv7_source_without_in_band_rpu_falls_back_to_the_plain_remux() {
    if !have_tools() {
        eprintln!(
            "SKIP a_dv7_source_without_in_band_rpu_falls_back: ffmpeg/ffprobe/libx265 missing"
        );
        return;
    }
    let dir = tmpdir("norpu");
    let src = build_fixture(&dir, false);
    let h = harness();
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let code = run_signaled(&h, &remux_args(&src, &out, None)).await;
    assert_eq!(code, 0);
    assert_eq!(
        dv81_count(&h, crate::metrics::Dv81Outcome::FallbackNoRpu),
        1
    );
    assert_eq!(dv81_count(&h, crate::metrics::Dv81Outcome::Converted), 0);
    // The plain remux ran: output exists, still describes the source (profile 7), no RPUs.
    let init = ffprobe(&[p(&out.join("init.mp4"))]);
    assert!(!init.contains("profile: 8"), "{init}");
    assert!(!output_nals(&out)
        .iter()
        .any(|n| n.nal_unit_type == RPU_NAL_UNIT_TYPE));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_dv_source_falls_back_as_not_p7() {
    if !have_tools() {
        eprintln!("SKIP a_non_dv_source_falls_back_as_not_p7: ffmpeg/ffprobe/libx265 missing");
        return;
    }
    let dir = tmpdir("notp7");
    let src = dir.join("sdr.mkv");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=s=256x144:r=24:d=2",
        "-f",
        "lavfi",
        "-i",
        "sine=d=2",
        "-c:v",
        "libx265",
        "-x265-params",
        "log-level=error",
        "-c:a",
        "aac",
        p(&src),
    ]);
    let h = harness();
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let code = run_signaled(&h, &remux_args(&src, &out, None)).await;
    assert_eq!(code, 0);
    assert_eq!(
        dv81_count(&h, crate::metrics::Dv81Outcome::FallbackNotP7),
        1
    );
    assert!(out.join("init.mp4").exists());
    let _ = std::fs::remove_dir_all(&dir);
}
