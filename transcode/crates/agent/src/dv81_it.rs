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

/// Whether the local `ffmpeg` supports `tcpool_ir::DV_REMOVAL_BSF`
/// (`hevc_metadata=remove_dovi=1`): a jellyfin-ffmpeg-only Debian patch
/// (`debian/patches/0061-add-remove-dovi-hdr10plus-bsf.patch`, confirmed present at tag
/// `v8.1.2-5`), not in stock ffmpeg. The two non-converting fallback tests below exec real ffmpeg
/// through it, so on a stock build (this workstation's Fedora ffmpeg, MEASURED 2026-09-27: `ffmpeg
/// -h bsf=hevc_metadata` lists no such option) they skip with a note rather than fail -- same
/// convention as `have_tools()`. The converting test never takes this path.
fn have_dv_removal_bsf() -> bool {
    StdCommand::new("ffmpeg")
        .args(["-hide_banner", "-h", "bsf=hevc_metadata"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("remove_dovi"))
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

/// Jellyfin's HLS fmp4 remux shape (video copy, audio transcode, -copyts -start_at_zero), carrying
/// the P5 marker pair and `-bsf:v hevc_mp4toannexb` -- the jellymesh PR #3 (bug-hunt patch 13)
/// argv shape. `run_dv81`/`run_signaled` must strip the marker before running anything, whichever
/// path (convert or fallback) it takes, and the bsf must survive unstripped.
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
            "-bsf:v",
            "hevc_mp4toannexb",
            "-metadata:s:v:0",
            "JELLYMESH_DOVI_P7_TO_81=1",
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
    harness_with(Config::minimal(), None)
}

fn harness_with(cfg: Config, detach: Option<Arc<Detach>>) -> Harness {
    let (_drain_tx, drain_rx) = watch::channel(false);
    let state = Arc::new(State {
        cfg,
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
        detach,
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
    if !have_dv_removal_bsf() {
        eprintln!(
            "SKIP a_dv7_source_without_in_band_rpu_falls_back: local ffmpeg lacks \
             hevc_metadata's remove_dovi (jellyfin-ffmpeg only, not stock ffmpeg)"
        );
        return;
    }
    let dir = tmpdir("norpu");
    let src = build_fixture(&dir, false);
    let h = harness();
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let code = run_signaled(&h, &remux_args(&src, &out, None)).await;
    let stderr = h.stderr.lock().unwrap().clone();
    assert_eq!(code, 0, "fallback pipeline failed:\n{stderr}");
    assert_eq!(
        dv81_count(&h, crate::metrics::Dv81Outcome::FallbackNoRpu),
        1
    );
    assert_eq!(dv81_count(&h, crate::metrics::Dv81Outcome::Converted), 0);
    // The fallback ran with DV removed (`add_dv_removal_bsf`), not a byte-for-byte plain remux:
    // this client was only ever told "8.1 or HDR10", and the source's real elementary stream has
    // no in-band RPU to serve as a genuine 8.1 -- so the output must carry no DOVI record at all
    // (neither the source's stale profile-7 tag nor a fabricated profile 8), and no RPU NAL. Also
    // no fabricated-config-record warning (the `dovi_rpu`-bsf failure mode docs/engineering/transcode-plan.md warns about on
    // an `hvcE` source with no in-band RPU -- `add_dv_removal_bsf` doesn't use that bsf, but this
    // is cheap, real regression coverage that removal, not fabrication, happened).
    assert!(!stderr.contains("Generating one"), "{stderr}");
    let init = ffprobe(&[p(&out.join("init.mp4"))]);
    assert!(!init.contains("DOVI configuration record"), "{init}");
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
    if !have_dv_removal_bsf() {
        eprintln!(
            "SKIP a_non_dv_source_falls_back_as_not_p7: local ffmpeg lacks hevc_metadata's \
             remove_dovi (jellyfin-ffmpeg only, not stock ffmpeg)"
        );
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
    let stderr = h.stderr.lock().unwrap().clone();
    assert_eq!(code, 0, "fallback failed:\n{stderr}");
    assert_eq!(
        dv81_count(&h, crate::metrics::Dv81Outcome::FallbackNotP7),
        1
    );
    assert!(out.join("init.mp4").exists());
    // This source has no Dolby Vision configuration record at all (docs/engineering/transcode-plan.md's "(b)" warning:
    // `dovi_rpu`-bsf on a stream with no config record can fabricate one, "Generating one, but
    // results may be invalid"). `strip=1` must not trigger that -- it removes rather than reads.
    assert!(
        !stderr.contains("Generating one"),
        "dovi_rpu fabricated a DV config record on a non-DV source:\n{stderr}"
    );
    let init = ffprobe(&[p(&out.join("init.mp4"))]);
    assert!(!init.contains("DOVI configuration record"), "{init}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// pool-r1 (jm8 follow-up): a detached DV7 -> 8.1 job must be throttleable like a plain one. The
/// orphan throttle's edge is the HLS muxer's `Opening '<stem>N.<ext>' for writing` on the pumped
/// stderr, and its keys go down the job's stdin channel; in the converting pipeline both must be
/// ffmpeg#2's (the HLS writer), never ffmpeg#1's (the demuxer: `-nostdin`, stderr to the log).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_detached_dv81_job_sees_segment_progress_and_its_keys_reach_the_hls_writer() {
    if !have_tools() {
        eprintln!("SKIP a_detached_dv81_job_sees_segment_progress: ffmpeg/ffprobe/libx265 missing");
        return;
    }
    const STEM: &str = "0123456789abcdef0123456789abcdef";
    let dir = tmpdir("detach");
    let src = build_fixture(&dir, true);
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    // Jellyfin's names: <stem>.m3u8, <stem>N.mp4, <stem>-1.mp4 (init).
    let args: Vec<String> = remux_args(&src, &out, None)
        .into_iter()
        .map(|a| {
            a.replace("seg%d.mp4", &format!("{STEM}%d.mp4"))
                .replace("main.m3u8", &format!("{STEM}.m3u8"))
                .replace("init.mp4", &format!("{STEM}-1.mp4"))
        })
        .collect();
    // Record what reaches each ffmpeg's stdin, keyed by which one it is (ffmpeg#2 reads pipe:3).
    let wrapper = dir.join("ff-keys");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\ncase \"$*\" in\n  *pipe:3*) exec 9<&0; ( cat <&9 > '{d}/keys-mux' ) & exec ffmpeg \"$@\" < /dev/null;;\n  *pipe:1*) exec 9<&0; ( cat <&9 > '{d}/keys-demux' ) & exec ffmpeg \"$@\" < /dev/null;;\nesac\nexec ffmpeg \"$@\"\n",
            d = p(&dir)
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut cfg = Config::minimal();
    cfg.ffmpeg = p(&wrapper).to_string();
    cfg.detach = true;
    cfg.policy.output_root = p(&dir).to_string();
    let pl = out.join(format!("{STEM}.m3u8"));
    std::fs::write(tcpool_ir::shared::lease_path(p(&pl)), "token").unwrap();
    let d = Detach::from_job(&cfg, &args, p(&dir.join("keepalive")))
        .expect("detachable")
        .expect("a lease and a Jellyfin-shaped output");
    let d = Arc::new(d);
    let h = harness_with(cfg, Some(d.clone()));
    // The throttle's keys, queued before ffmpeg#2 exists: whoever drains the channel gets them.
    h._stdin_tx.send(Some(b"p".to_vec())).await.unwrap();
    h._stdin_tx.send(Some(b"u".to_vec())).await.unwrap();

    let code = run_signaled(&h, &args).await;
    let stderr = h.stderr.lock().unwrap().clone();
    assert_eq!(code, 0, "converting pipeline failed:\n{stderr}");
    assert_eq!(dv81_count(&h, crate::metrics::Dv81Outcome::Converted), 1);

    let segs = std::fs::read_dir(&out)
        .unwrap()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.strip_prefix(STEM)?
                .strip_suffix(".mp4")?
                .parse::<u64>()
                .ok()
        })
        .max()
        .expect("segments written");
    assert!(
        segs >= 2,
        "fixture too short for the check: last segment {segs}"
    );
    // Opening the last segment (index `segs`) means 0..segs-1 are complete.
    assert_eq!(
        d.newest_written(),
        Some(segs - 1),
        "the throttle edge must come from ffmpeg#2's stderr:\n{stderr}"
    );
    let read = |n: &str| {
        let f = dir.join(n);
        for _ in 0..40 {
            if let Ok(t) = std::fs::read_to_string(&f) {
                if !t.is_empty() {
                    return t;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        std::fs::read_to_string(&f).unwrap_or_default()
    };
    assert_eq!(
        read("keys-mux"),
        "pu",
        "p/u must reach ffmpeg#2 (the HLS writer)"
    );
    assert_eq!(read("keys-demux"), "", "ffmpeg#1 must not get the keys");
    let _ = std::fs::remove_dir_all(&dir);
}

/// One sample entry of an fMP4 init segment: its fourcc, its child box fourccs, and the
/// (profile, BL signal compatibility id, EL present) of a `dvcC`/`dvvC` child if it has one.
#[derive(Debug)]
struct SampleEntry {
    fourcc: String,
    children: Vec<String>,
    dovi: Option<(u8, u8, bool)>,
}

/// Walk `moov/trak/mdia/minf/stbl/stsd` of an init segment (enough ISO BMFF for the checks
/// below; not a general parser).
fn sample_entries(init: &[u8]) -> Vec<SampleEntry> {
    fn boxes(buf: &[u8]) -> Vec<(String, &[u8])> {
        let mut out = Vec::new();
        let mut off = 0;
        while off + 8 <= buf.len() {
            let size = u32::from_be_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
            let name = String::from_utf8_lossy(&buf[off + 4..off + 8]).into_owned();
            if size < 8 || off + size > buf.len() {
                break;
            }
            out.push((name, &buf[off + 8..off + size]));
            off += size;
        }
        out
    }
    fn find<'a>(buf: &'a [u8], path: &[&str]) -> Vec<&'a [u8]> {
        let Some((first, rest)) = path.split_first() else {
            return vec![buf];
        };
        boxes(buf)
            .into_iter()
            .filter(|(n, _)| n == first)
            .flat_map(|(_, b)| find(b, rest))
            .collect()
    }
    let mut out = Vec::new();
    for stsd in find(init, &["moov", "trak", "mdia", "minf", "stbl", "stsd"]) {
        for (fourcc, body) in boxes(stsd.get(8..).unwrap_or_default()) {
            // Visual sample entries have 78 bytes before their child boxes, audio ones 28.
            let skip = if matches!(fourcc.as_str(), "hvc1" | "hev1" | "dvh1" | "dvhe") {
                78
            } else {
                28
            };
            let kids = boxes(body.get(skip..).unwrap_or_default());
            let dovi = kids
                .iter()
                .find(|(n, b)| (n == "dvcC" || n == "dvvC") && b.len() >= 5)
                .map(|(_, b)| (b[2] >> 1, b[4] >> 4, (b[3] >> 1) & 1 == 1));
            out.push(SampleEntry {
                fourcc,
                children: kids.into_iter().map(|(n, _)| n).collect(),
                dovi,
            });
        }
    }
    out
}

/// Bughunt 17's argv shape: Jellyfin serves a converted profile-7 source as fMP4, tags the video
/// `hvc1 -strict -2` (`EncodingHelper.GetDoviP7ToP81CodecTagArgs`) and copies a declared TrueHD
/// track (`-codec:a:0 copy -strict -2`). Derived from `remux_args`.
fn remux_args_fmp4_copy(input: &Path, out: &Path) -> Vec<String> {
    let src = remux_args(input, out, None);
    let mut a = Vec::with_capacity(src.len() + 4);
    let mut i = 0;
    while i < src.len() {
        match (src[i].as_str(), src.get(i + 1).map(String::as_str)) {
            ("-tag:v:0", Some(_)) => {
                a.extend(["-tag:v:0", "hvc1", "-strict", "-2"].map(String::from));
                i += 2;
            }
            ("-codec:a:0", Some(_)) => {
                a.extend(["-codec:a:0", "copy", "-strict", "-2"].map(String::from));
                i += 2;
            }
            ("-ac", Some(_)) => i += 2,
            _ => {
                a.push(src[i].clone());
                i += 1;
            }
        }
    }
    a
}

/// What the Android TV app (media3) and AVPlayer need from a converted fMP4 output: an `hvc1`
/// sample entry with a `dvvC` box for profile 8 / BL compatibility 1 / no EL (media3's
/// `BoxParser` turns that into `video/dolby-vision`; its TS extractor only ever reports
/// `video/hevc`), and the TrueHD track copied as `mlpa`.
fn assert_dv81_fmp4_init(dir: &Path, tag: &str) {
    let entries = sample_entries(&std::fs::read(dir.join("init.mp4")).unwrap());
    let video = entries
        .iter()
        .find(|e| e.fourcc.starts_with("hv") || e.fourcc.starts_with("dvh"))
        .unwrap_or_else(|| panic!("[{tag}] no video sample entry: {entries:?}"));
    assert_eq!(video.fourcc, "hvc1", "[{tag}] {entries:?}");
    assert!(
        video.children.iter().any(|c| c == "dvvC") && !video.children.iter().any(|c| c == "dvcC"),
        "[{tag}] profile 8 needs dvvC, not dvcC: {entries:?}"
    );
    assert_eq!(video.dovi, Some((8, 1, false)), "[{tag}] {entries:?}");
    assert!(
        entries.iter().any(|e| e.fourcc == "mlpa"),
        "[{tag}] TrueHD not copied as mlpa: {entries:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn converted_fmp4_is_hvc1_with_dvvc_and_keeps_truehd() {
    if !have_tools() {
        eprintln!(
            "SKIP converted_fmp4_is_hvc1_with_dvvc_and_keeps_truehd: ffmpeg/ffprobe/libx265 missing"
        );
        return;
    }
    let dir = tmpdir("fmp4thd");
    let fixture = build_fixture(&dir, true);
    // Same profile-7 video; audio re-encoded to TrueHD (ffmpeg's encoder is experimental).
    let src = dir.join("fixture-truehd.mkv");
    ffmpeg(&[
        "-i",
        p(&fixture),
        "-map",
        "0",
        "-c:v",
        "copy",
        "-c:a",
        "truehd",
        "-strict",
        "-2",
        p(&src),
    ]);
    let h = harness();
    let out = dir.join("dv");
    std::fs::create_dir_all(&out).unwrap();
    let code = run_signaled(&h, &remux_args_fmp4_copy(&src, &out)).await;
    let stderr = h.stderr.lock().unwrap().clone();
    assert_eq!(code, 0, "converting pipeline failed:\n{stderr}");
    assert_eq!(dv81_count(&h, crate::metrics::Dv81Outcome::Converted), 1);
    assert_dv81_fmp4_init(&out, "synthetic");

    // Every RPU is profile 8 now, and every TrueHD packet made it through.
    let rpus: Vec<_> = output_nals(&out)
        .into_iter()
        .filter(|n| n.nal_unit_type == RPU_NAL_UNIT_TYPE)
        .collect();
    assert!(!rpus.is_empty());
    for r in &rpus {
        let rpu = DoviRpu::parse_unspec62_nalu(&r.data[2..]).expect("RPU parses");
        assert_eq!(rpu.dovi_profile, 8);
    }
    let audio = |f: &Path| {
        ffprobe(&[
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-count_packets",
            "-show_entries",
            "stream=codec_name,nb_read_packets",
            "-of",
            "csv=p=0",
            p(f),
        ])
    };
    let got = audio(&concat_hls(&out));
    assert!(got.starts_with("truehd,"), "audio: {got}");
    assert_eq!(got, audio(&src), "TrueHD packets lost");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The same init-segment check on a real profile-7 file with in-band RPU, video on stream 0 and
/// TrueHD on stream 1: `DV81_SOURCE=/path/to/file.mkv cargo test -p tcpool-agent
/// converted_fmp4_real_source_from_env -- --nocapture`. Skipped when the variable is unset.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn converted_fmp4_real_source_from_env() {
    let Some(src) = std::env::var_os("DV81_SOURCE").map(PathBuf::from) else {
        eprintln!("SKIP converted_fmp4_real_source_from_env: DV81_SOURCE not set");
        return;
    };
    if !have_tools() {
        eprintln!("SKIP converted_fmp4_real_source_from_env: ffmpeg/ffprobe missing");
        return;
    }
    let dir = tmpdir("fmp4real");
    let h = harness();
    let out = dir.join("dv");
    std::fs::create_dir_all(&out).unwrap();
    let code = run_signaled(&h, &remux_args_fmp4_copy(&src, &out)).await;
    let stderr = h.stderr.lock().unwrap().clone();
    assert_eq!(code, 0, "converting pipeline failed:\n{stderr}");
    assert_eq!(dv81_count(&h, crate::metrics::Dv81Outcome::Converted), 1);
    assert_dv81_fmp4_init(&out, "real");
    eprintln!(
        "real source init: {:?}",
        sample_entries(&std::fs::read(out.join("init.mp4")).unwrap())
    );
    let _ = std::fs::remove_dir_all(&dir);
}
