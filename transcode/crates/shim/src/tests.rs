//! Unit tests for worker discovery and ranking (the parts that are pure functions).

use super::*;

fn target(name: &str, addr: &str) -> Target {
    Target {
        name: name.into(),
        addr: addr.into(),
    }
}

#[test]
fn dns_target_defaults_to_the_agent_port() {
    assert_eq!(
        parse_dns_target("tcpool-agents.media.svc.cluster.local."),
        Some((
            "tcpool-agents.media.svc.cluster.local.".into(),
            tcpool_proto::DEFAULT_PORT
        ))
    );
}

#[test]
fn dns_target_keeps_the_trailing_dot_and_reads_the_port() {
    assert_eq!(
        parse_dns_target("tcpool-agents.media.svc.cluster.local.:9901"),
        Some(("tcpool-agents.media.svc.cluster.local.".into(), 9901))
    );
    assert_eq!(
        parse_dns_target(" tcpool-agents:19901 "),
        Some(("tcpool-agents".into(), 19901))
    );
}

#[test]
fn dns_target_accepts_a_bracketed_v6_literal() {
    assert_eq!(
        parse_dns_target("[fd00::1]:9901"),
        Some(("fd00::1".into(), 9901))
    );
    assert_eq!(
        parse_dns_target("[fd00::1]"),
        Some(("fd00::1".into(), tcpool_proto::DEFAULT_PORT))
    );
}

#[test]
fn dns_target_refuses_what_it_cannot_read_unambiguously() {
    // unset, a bare v6 literal (is the last group a port?), a junk port, no host
    assert_eq!(parse_dns_target(""), None);
    assert_eq!(parse_dns_target("   "), None);
    assert_eq!(parse_dns_target("fd00::1:9901"), None);
    assert_eq!(parse_dns_target("tcpool-agents:not-a-port"), None);
    assert_eq!(parse_dns_target(":9901"), None);
    assert_eq!(parse_dns_target("[]:9901"), None);
}

#[test]
fn static_workers_fill_in_addresses_dns_did_not_return() {
    let dns = vec![target("10.42.0.7:9901", "10.42.0.7:9901")];
    let statics = vec![
        // same address, already known from DNS: not duplicated
        ("qsv".to_string(), "10.42.0.7:9901".to_string()),
        ("nv".to_string(), "10.42.1.9:9901".to_string()),
    ];
    assert_eq!(
        merge_targets(dns, statics),
        vec![
            target("10.42.0.7:9901", "10.42.0.7:9901"),
            target("nv", "10.42.1.9:9901"),
        ]
    );
}

#[test]
fn merge_keeps_the_static_list_alone_when_dns_is_unset() {
    assert_eq!(
        merge_targets(Vec::new(), vec![("cpu".into(), "127.0.0.1:9901".into())]),
        vec![target("cpu", "127.0.0.1:9901")]
    );
}

/// `connect_lazy` dials nothing, but it does need a reactor in scope, so the tests that build a
/// `Worker` are `#[tokio::test]`. `rank()` itself is synchronous and only reads `caps`.
fn worker(name: &str, kind: &str, capacity: f64, used: f64, order: usize) -> Worker {
    Worker {
        name: name.into(),
        channel: Endpoint::from_static("http://127.0.0.1:1").connect_lazy(),
        caps: Caps {
            name: name.into(),
            kind: kind.into(),
            outputs: vec!["h264".into()],
            capacity,
            units_used: used,
            ..Default::default()
        },
        order,
    }
}

#[tokio::test]
async fn an_idle_pool_prefers_a_gpu_over_the_cpu_spill() {
    // Everything free, and DNS handed the CPU worker back first: kind must break the tie.
    let ws = vec![
        worker("cpu", "cpu", 4.0, 0.0, 0),
        worker("nv", "nvenc", 6.0, 0.0, 1),
        worker("qsv", "qsv", 14.0, 0.0, 2),
    ];
    let names: Vec<String> = rank(ws, Some("h264"), true)
        .iter()
        .map(|w| w.label().to_string())
        .collect();
    assert_eq!(names, vec!["nv", "qsv", "cpu"]);
}

#[tokio::test]
async fn free_capacity_still_wins_over_kind() {
    let ws = vec![
        worker("qsv", "qsv", 14.0, 13.0, 0), // 7% free
        worker("cpu", "cpu", 4.0, 0.0, 1),   // idle
    ];
    let names: Vec<String> = rank(ws, None, true)
        .iter()
        .map(|w| w.label().to_string())
        .collect();
    assert_eq!(names, vec!["cpu", "qsv"]);
}

#[tokio::test]
async fn a_worker_without_the_required_output_is_dropped() {
    let ws = vec![worker("qsv", "qsv", 14.0, 0.0, 0)];
    assert!(rank(ws, Some("av1"), true).is_empty());
}

#[tokio::test]
async fn the_log_label_falls_back_to_the_dialled_address() {
    let mut w = worker("10.42.0.7:9901", "qsv", 14.0, 0.0, 0);
    w.caps.name = String::new();
    assert_eq!(w.label(), "10.42.0.7:9901");
}

/// PLAYBACK ranking (discount_batch=true) must not see a worker's currently-running-but-
/// preemptible BATCH load as firm demand: two GPU workers each half-loaded with BATCH jobs
/// should still rank ahead of an idle CPU spill worker, since preempting either GPU's batch job
/// frees identical or more capacity than the CPU spill ever has.
#[tokio::test]
async fn playback_ranking_discounts_running_batch_load() {
    let mut gpu_a = worker("gpu-a", "qsv", 4.0, 2.0, 0);
    gpu_a.caps.batch_units_used = 2.0; // all of its used units are batch, so effective load = 0
    let mut gpu_b = worker("gpu-b", "nvenc", 4.0, 2.0, 1);
    gpu_b.caps.batch_units_used = 2.0;
    let cpu = worker("cpu", "cpu", 4.0, 0.0, 2); // idle, but the CPU spill

    let names: Vec<String> = rank(vec![gpu_a, gpu_b, cpu], None, true)
        .iter()
        .map(|w| w.label().to_string())
        .collect();
    assert_eq!(
        names,
        vec!["gpu-a", "gpu-b", "cpu"],
        "GPU workers carrying only preemptible batch load must rank as free as an idle CPU spill, \
         with kind_rank breaking the tie in the GPUs' favor"
    );
}

/// BATCH's own ranking keeps raw units_used (discount_batch=false): a worker already carrying
/// batch load must NOT look more free than it really is to another batch candidate.
#[tokio::test]
async fn batch_ranking_does_not_discount_its_own_load() {
    let mut loaded = worker("loaded", "qsv", 4.0, 3.0, 0);
    loaded.caps.batch_units_used = 3.0; // all of it is batch, but batch ranking must ignore that
    let idle = worker("idle", "cpu", 4.0, 0.0, 1);

    let names: Vec<String> = rank(vec![loaded, idle], None, false)
        .iter()
        .map(|w| w.label().to_string())
        .collect();
    assert_eq!(names, vec!["idle", "loaded"]);
}

/// `apply_affinity` present + capable + free -> moved to the front, ahead of a worker `rank`
/// alone would have preferred (more free capacity, GPU-before-CPU tie-break).
#[tokio::test]
async fn apply_affinity_moves_a_capable_free_worker_to_the_front() {
    let ws = rank(
        vec![
            worker("nv", "nvenc", 4.0, 0.0, 0), // idle: rank() alone would put this first
            worker("qsv", "qsv", 4.0, 3.0, 1),  // the pinned worker, only 25% free
        ],
        Some("h264"),
        true,
    );
    let names: Vec<String> = apply_affinity(ws, "qsv")
        .iter()
        .map(|w| w.label().to_string())
        .collect();
    assert_eq!(names, vec!["qsv", "nv"]);
}

/// Absent (never Hello'd this round, or filtered out by `rank`'s `need` check -- i.e.
/// "incapable") -> the ranked order is left exactly as `rank` produced it.
#[tokio::test]
async fn apply_affinity_leaves_order_alone_when_the_pinned_worker_is_absent() {
    let ws = rank(
        vec![
            worker("cpu", "cpu", 4.0, 0.0, 0),
            worker("qsv", "qsv", 4.0, 0.0, 1),
        ],
        Some("h264"),
        true,
    );
    let before: Vec<String> = ws.iter().map(|w| w.label().to_string()).collect();
    let after: Vec<String> = apply_affinity(ws, "gone")
        .iter()
        .map(|w| w.label().to_string())
        .collect();
    assert_eq!(after, before);
}

/// Present but the wrong `need` (can't output what's asked) never survives `rank`'s filter into
/// `ws`, so it reads to `apply_affinity` exactly like "absent": normal order, not a panic or a
/// worker pulled in from outside the candidate list.
#[tokio::test]
async fn apply_affinity_treats_an_incapable_pinned_worker_as_absent() {
    let h264_only = worker("h264only", "qsv", 4.0, 0.0, 0); // default outputs = ["h264"]
    let mut hevc_capable = worker("hevc", "nvenc", 4.0, 0.0, 1);
    hevc_capable.caps.outputs = vec!["hevc".into()];
    let ws = rank(vec![h264_only, hevc_capable], Some("hevc"), true);
    let names: Vec<String> = apply_affinity(ws, "h264only")
        .iter()
        .map(|w| w.label().to_string())
        .collect();
    assert_eq!(names, vec!["hevc"]); // h264only was dropped by rank(), never reinserted
}

/// Present but fully loaded (no free capacity) -> left where `rank` put it, not pulled to the
/// front; a `Busy` reply from it would just cost one round-trip for nothing gained.
#[tokio::test]
async fn apply_affinity_leaves_a_full_pinned_worker_where_rank_put_it() {
    let ws = rank(
        vec![
            worker("qsv", "qsv", 4.0, 4.0, 0), // 0% free
            worker("cpu", "cpu", 4.0, 0.0, 1), // idle
        ],
        None,
        true,
    );
    let names: Vec<String> = apply_affinity(ws, "qsv")
        .iter()
        .map(|w| w.label().to_string())
        .collect();
    assert_eq!(names, vec!["cpu", "qsv"], "full pinned worker not promoted");
}

#[test]
fn batch_routing_picks_priority_by_shape() {
    assert_eq!(
        priority_for(tcpool_ir::Shape::Trickplay),
        tcpool_proto::Priority::Batch
    );
    assert_eq!(
        priority_for(tcpool_ir::Shape::Hls),
        tcpool_proto::Priority::Playback
    );
    assert_eq!(
        priority_for(tcpool_ir::Shape::Other),
        tcpool_proto::Priority::Playback
    );
}

/// A scratch directory under the OS temp dir, removed on drop -- no extra dev-dependency needed
/// for the handful of filesystem-shaped tests below.
struct TempScratch(std::path::PathBuf);

impl TempScratch {
    fn new(tag: &str) -> TempScratch {
        let dir = std::env::temp_dir().join(format!(
            "tcpool-shim-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        TempScratch(dir)
    }

    fn path(&self, rel: &str) -> std::path::PathBuf {
        self.0.join(rel)
    }
}

impl Drop for TempScratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn under_trickplay_root_accepts_a_not_yet_created_subdir_of_the_root() {
    let t = TempScratch::new("accept");
    let root = t.path("root");
    std::fs::create_dir_all(&root).unwrap();
    // Jellyfin creates the per-call guid dir right before running ffmpeg; it need not exist yet.
    let dir = root.join("guid1");
    assert!(under_trickplay_root(
        dir.to_str().unwrap(),
        root.to_str().unwrap()
    ));
}

#[test]
fn under_trickplay_root_rejects_a_lexical_dotdot() {
    let t = TempScratch::new("dotdot");
    let root = t.path("root");
    std::fs::create_dir_all(&root).unwrap();
    let escaping = format!("{}/../escape/guid1", root.display());
    assert!(!under_trickplay_root(&escaping, root.to_str().unwrap()));
}

#[test]
fn under_trickplay_root_rejects_a_relative_path() {
    assert!(!under_trickplay_root("relative/guid1", "/tmp"));
}

#[test]
fn under_trickplay_root_rejects_an_unconfigured_root() {
    let t = TempScratch::new("noroot");
    let missing_root = t.path("does-not-exist");
    let dir = missing_root.join("guid1");
    assert!(!under_trickplay_root(
        dir.to_str().unwrap(),
        missing_root.to_str().unwrap()
    ));
}

#[test]
fn under_trickplay_root_rejects_a_sibling_that_merely_shares_a_prefix() {
    let t = TempScratch::new("prefix");
    let root = t.path("root");
    std::fs::create_dir_all(&root).unwrap();
    let sibling = t.path("root-evil").join("guid1"); // "root-evil" is not under "root"
    assert!(!under_trickplay_root(
        sibling.to_str().unwrap(),
        root.to_str().unwrap()
    ));
}

#[cfg(unix)]
#[test]
fn under_trickplay_root_follows_a_symlinked_ancestor_out_of_the_root() {
    let t = TempScratch::new("symlink");
    let root = t.path("root");
    let outside = t.path("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    // root/escape -> ../outside : an existing ancestor whose real location is outside the root.
    std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
    let dir = root.join("escape").join("guid1");
    assert!(!under_trickplay_root(
        dir.to_str().unwrap(),
        root.to_str().unwrap()
    ));
}

#[cfg(unix)]
#[test]
fn under_trickplay_root_accepts_a_symlinked_root_itself() {
    let t = TempScratch::new("symlinked-root");
    let real = t.path("real-root");
    let root = t.path("root-link");
    std::fs::create_dir_all(&real).unwrap();
    std::os::unix::fs::symlink(&real, &root).unwrap();
    let dir = root.join("guid1");
    assert!(under_trickplay_root(
        dir.to_str().unwrap(),
        root.to_str().unwrap()
    ));
}

/// A1 (REVISED): `batch_pool_failure`'s no-rerun exit code must not collide with any other exit
/// the shim returns, nor with the `code & 0xff` a killed process's negative signal number can
/// produce (SIGKILL's -9 -> 247 is the only one observed in this codebase today).
#[test]
fn batch_partial_failure_exit_collides_with_nothing_reserved() {
    let reserved = [0, 127, 247, 255];
    assert!(
        !reserved.contains(&BATCH_PARTIAL_FAILURE_EXIT),
        "BATCH_PARTIAL_FAILURE_EXIT ({BATCH_PARTIAL_FAILURE_EXIT}) collides with a reserved shim exit code"
    );
}

/// Args for a trickplay job writing an `%08d.jpg` sequence into `dir`, honouring `start_number`
/// like `render_trickplay`'s real argv shape (mjpeg last-arg-is-pattern).
fn trickplay_args(dir: &std::path::Path, start_number: Option<u64>) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        "in.mkv".into(),
        "-vf".into(),
        "fps=1".into(),
        "-c:v".into(),
        "mjpeg".into(),
    ];
    if let Some(n) = start_number {
        a.push("-start_number".into());
        a.push(n.to_string());
    }
    a.push(format!("{}/%08d.jpg", dir.display()));
    a
}

#[test]
fn batch_pool_failure_reruns_locally_when_no_directory_exists_yet() {
    let t = TempScratch::new("no-dir");
    let args = trickplay_args(&t.path("frames"), None);
    assert_eq!(batch_pool_failure(&args, "was lost"), Outcome::Local);
}

#[test]
fn batch_pool_failure_reruns_locally_when_the_directory_is_empty() {
    let t = TempScratch::new("empty-dir");
    let dir = t.path("frames");
    std::fs::create_dir_all(&dir).unwrap();
    let args = trickplay_args(&dir, None);
    assert_eq!(batch_pool_failure(&args, "was lost"), Outcome::Local);
}

#[test]
fn batch_pool_failure_exits_without_a_rerun_once_the_first_frame_exists() {
    let t = TempScratch::new("first-frame");
    let dir = t.path("frames");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("00000001.jpg"), b"jpg").unwrap();
    let args = trickplay_args(&dir, None);
    assert_eq!(
        batch_pool_failure(&args, "exited 1"),
        Outcome::Exit(BATCH_PARTIAL_FAILURE_EXIT)
    );
}

/// `-start_number` must be honoured, not "any jpg in the directory": a stray frame at the
/// default start (1) while the job actually started numbering at 5 must NOT count as "a first
/// frame exists".
#[test]
fn batch_pool_failure_honours_start_number_not_any_jpg() {
    let t = TempScratch::new("start-number");
    let dir = t.path("frames");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("00000001.jpg"), b"jpg").unwrap();
    let args = trickplay_args(&dir, Some(5));
    assert_eq!(batch_pool_failure(&args, "was lost"), Outcome::Local);

    std::fs::write(dir.join("00000005.jpg"), b"jpg").unwrap();
    assert_eq!(
        batch_pool_failure(&args, "was lost"),
        Outcome::Exit(BATCH_PARTIAL_FAILURE_EXIT)
    );
}

#[test]
fn trickplay_frame_count_counts_only_jpg_files_in_the_output_dir() {
    let t = TempScratch::new("frame-count");
    let dir = t.path("frames");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("00000001.jpg"), b"jpg").unwrap();
    std::fs::write(dir.join("00000002.JPG"), b"jpg").unwrap();
    std::fs::write(dir.join("00000003.jpeg"), b"jpg").unwrap();
    std::fs::write(dir.join("notes.txt"), b"x").unwrap();
    let args = trickplay_args(&dir, None);
    assert_eq!(trickplay_frame_count(&args), 3);
}

#[test]
fn trickplay_frame_count_is_zero_when_the_directory_does_not_exist() {
    let t = TempScratch::new("frame-count-missing");
    let args = trickplay_args(&t.path("nope"), None);
    assert_eq!(trickplay_frame_count(&args), 0);
}

// ---------------------------------------------------------------------------------------------
// P5: the local (no-pool) fallback must never copy raw DV profile 7 under a marker that told
// Jellyfin's client it would get 8.1 -- `dv81_local_fallback_args` is the one sanctioned rewrite
// of `exec_real`'s otherwise-unmodified argv.
// ---------------------------------------------------------------------------------------------

fn osv(v: &[&str]) -> Vec<OsString> {
    v.iter().map(OsString::from).collect()
}

/// A minimal Jellyfin-shaped HLS remux argv carrying the P5 marker and (optionally) an existing
/// `-bsf:v` chain, ending in the output `.m3u8`.
fn dv81_signaled_args(existing_bsf: Option<&str>) -> Vec<OsString> {
    let mut v = vec![
        "-f".to_string(),
        "matroska,webm".to_string(),
        "-i".to_string(),
        "file:/data/media/movies/X/X.mkv".to_string(),
        "-map".to_string(),
        "0:0".to_string(),
        "-codec:v:0".to_string(),
        "copy".to_string(),
    ];
    if let Some(chain) = existing_bsf {
        v.extend(["-bsf:v".to_string(), chain.to_string()]);
    }
    v.extend([
        "-metadata:s:v:0".to_string(),
        "JELLYMESH_DOVI_P7_TO_81=1".to_string(),
        "-copyts".to_string(),
        "-f".to_string(),
        "hls".to_string(),
        "-y".to_string(),
        "/transcodes/jf/abc.m3u8".to_string(),
    ]);
    v.into_iter().map(OsString::from).collect()
}

#[test]
fn dv81_local_fallback_leaves_an_unsignaled_argv_byte_for_byte_unchanged() {
    let raw = osv(&[
        "-i",
        "file:/data/media/movies/X/X.mkv",
        "-codec:v:0",
        "copy",
        "-y",
        "/transcodes/jf/abc.m3u8",
    ]);
    assert_eq!(dv81_local_fallback_args(&raw), raw);
}

#[test]
fn dv81_local_fallback_strips_the_marker_and_adds_a_dv_removal_bsf_when_none_exists() {
    let raw = dv81_signaled_args(None);
    let out = dv81_local_fallback_args(&raw);
    let s: Vec<String> = out
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(!wants_dv81(&s), "marker must be gone: {s:?}");
    assert!(
        s.windows(2)
            .any(|w| w == ["-bsf:v", tcpool_ir::DV_REMOVAL_BSF]),
        "DV-removal bsf missing: {s:?}"
    );
    assert_eq!(s.last().unwrap(), "/transcodes/jf/abc.m3u8");
}

#[test]
fn dv81_local_fallback_merges_into_an_existing_bsf_chain_not_a_second_flag() {
    let raw = dv81_signaled_args(Some("hevc_mp4toannexb"));
    let out = dv81_local_fallback_args(&raw);
    let s: Vec<String> = out
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(!wants_dv81(&s));
    let bsf_count = s.iter().filter(|a| a.as_str() == "-bsf:v").count();
    assert_eq!(bsf_count, 1, "must merge, not add a second -bsf:v: {s:?}");
    let i = s.iter().position(|a| a == "-bsf:v").unwrap();
    assert_eq!(
        s[i + 1],
        format!("hevc_mp4toannexb,{}", tcpool_ir::DV_REMOVAL_BSF)
    );
}
