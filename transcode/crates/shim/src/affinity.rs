//! Seek affinity: once a PLAYBACK session picks a worker, keep it parked there through seeks and
//! (to the extent the client actually reuses `PlaySessionId` across them -- see the caveat below)
//! audio/subtitle track changes and bitrate switches, instead of re-ranking fresh every restart --
//! a fresh rank on an ordinary seek can hop the session between the Arc (QSV) and the P4 (NVENC)
//! and show a visible quality step. Still fails over seamlessly: the pin is cleared before the
//! shim exits non-zero for a lost/fenced/watchdog-killed worker, so a restart after that never
//! returns to a half-dead card.
//!
//! Key: the HLS output prefix `lease::lease_path` already derives its lock file from. Jellyfin
//! computes that prefix as `MD5(MediaPath-UserAgent-DeviceId-PlaySessionId)`
//! (`Jellyfin.Api/Helpers/StreamingHelpers.cs:377-386`, vendored at
//! `~/.cache/jellymesh-vendor/jellyfin-src`), and every `DynamicHlsController` GET handler that
//! restarts ffmpeg for a seek or a track change carries the *same* `playSessionId` query param
//! straight into `state.PlaySessionId` (e.g. `DynamicHlsController.cs:229,469,639,808,975,1157,
//! 1337` -- one call site per HLS endpoint) rather than minting a new one, so the `<md5>` prefix
//! is stable **server-side** across any restart that carries an existing `playSessionId` query
//! param, and unrelated PLAYBACK sessions (different `PlaySessionId`, or a different
//! device/user-agent/title) always land on a different prefix and a different affinity file.
//! This was checked in the vendored server source only -- it says nothing about which restarts
//! the *client* (jellyfin-web) actually re-issues with the old `playSessionId` versus which ones
//! it treats as a new session. Whether a seek, or a client-driven audio/subtitle track or bitrate
//! change, reuses `PlaySessionId` rather than the client re-invoking `/PlaybackInfo` (which mints
//! a fresh `PlaySessionId` every time -- `MediaInfoHelper.cs:132`, `Guid.NewGuid()`) was **not
//! verified for any of the three** -- no jellyfin-web client source is vendored here to check,
//! and it was not independently re-traced (see docs/PLAN.md §10). The suite's own case 20b, which
//! restarts with a different `-start_number` on a playlist path the test script controls
//! directly, proves the shim's handling of a repeated `playSessionId`, not that a real client
//! actually sends one on a seek. If the client instead re-invokes `/PlaybackInfo` for any of
//! these restart types, that restart silently gets a fresh `<md5>` and a fresh `rank()` --
//! exactly the visible-hop behavior this feature exists to prevent, for that restart type.
//!
//! Store: a small file (`<md5>.worker`, sibling of the playlist and the lease lock, so every
//! Jellyfin replica sharing the scratch dir sees it) holding the pinned `Worker.name` identity
//! (not the display label two DaemonSet pods of the same class can share -- see `Worker::label`'s
//! doc comment), written atomically (tmp + rename) on every PLAYBACK `Accepted`. Never written or
//! read for BATCH (trickplay): `run_batch` never touches this module.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Default TTL: long enough to survive a paused/idle session, short enough that an orphaned file
/// (a delete race, a crash before cleanup) does not pin new sessions to a decommissioned worker
/// forever.
const DEFAULT_TTL: Duration = Duration::from_secs(6 * 3600);

fn ttl_var(name: &str) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_TTL)
}

/// `TC_AFFINITY_TTL_SECS`, or `DEFAULT_TTL` if unset/unparsable.
pub fn ttl_from_env() -> Duration {
    ttl_var("TC_AFFINITY_TTL_SECS")
}

fn enabled_var(name: &str) -> bool {
    std::env::var(name).ok().as_deref() != Some("0")
}

/// `TC_AFFINITY=0` disables the feature outright (default on): no read, no write, no clear.
pub fn enabled() -> bool {
    enabled_var("TC_AFFINITY")
}

/// The sibling file next to the playlist that `lease::lease_path` derives its lock file from --
/// same directory, same `<md5>` stem, so it travels with the output across replicas.
pub fn affinity_path(playlist: &str) -> PathBuf {
    let p = Path::new(playlist);
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    p.with_file_name(format!("{stem}.worker"))
}

/// The worker name pinned for this output, if the file exists, is younger than `ttl`, and its
/// content parses as a name. Any read/stat/parse failure is a miss, never a hard error: affinity
/// is an optimization on top of `rank()`, not a correctness requirement.
pub fn read(path: &Path, ttl: Duration) -> Option<String> {
    // A missing file is the common case (every session's first start, or the feature just turned
    // on) -- not worth a log line. Every other miss reason below gets one (item 7: hit/miss(reason)).
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    // Replica clocks can skew; a "negative" age (mtime in the future) is still fresh, not stale.
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::ZERO);
    if age >= ttl {
        crate::log(&format!(
            "affinity miss (stale): {} is {age:?} old (ttl {ttl:?})",
            path.display()
        ));
        return None;
    }
    let mut s = String::new();
    if File::open(path)
        .ok()
        .and_then(|mut f| f.read_to_string(&mut s).ok())
        .is_none()
    {
        crate::log(&format!(
            "affinity miss (garbage): {} unreadable or not UTF-8",
            path.display()
        ));
        return None;
    }
    let name = s.trim();
    if name.is_empty() {
        crate::log(&format!(
            "affinity miss (garbage): {} is empty/whitespace",
            path.display()
        ));
        return None;
    }
    Some(name.to_string())
}

/// Pin `name` for this output: write tmp + rename, so a concurrent reader (another Jellyfin
/// replica, or this shim's own next round) never observes a partial write. The tmp name carries
/// our pid so two shims racing to reach `Accepted` for the same output (a before-first-segment
/// retry landing on a different worker, or the losing side of `lease::try_acquire`) never
/// collide on the same tmp path.
pub fn write(path: &Path, name: &str) {
    let tmp_name = format!(
        "{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    );
    let tmp = path.with_file_name(tmp_name);
    let result = (|| -> std::io::Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(name.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        crate::log(&format!(
            "affinity: writing {} for worker {name}: {e}",
            path.display()
        ));
        let _ = fs::remove_file(&tmp);
    }
}

/// Drop the pin: called only on the unclean-loss/fence path to a non-zero exit, so the restart
/// ranks fresh instead of returning to a card that just died. A clean Busy/refusal from the
/// affine worker leaves the file alone and falls through to the normal ranked order.
pub fn clear(path: &Path) {
    let _ = fs::remove_file(path);
}

/// How often `sweep_stale` actually walks `dir`: gated behind a throttle marker so a shim that
/// restarts every few seconds within one long PLAYBACK session (a seek, a track change) pays one
/// extra `stat` per start, not a `readdir`, unless this much time has actually passed.
/// Independent of the caller's `ttl` -- this only bounds sweep *frequency*, not staleness.
const SWEEP_INTERVAL: Duration = Duration::from_secs(600);

/// Delete `<md5>.worker` files in `dir` (and any `<md5>.worker.<pid>.tmp` left behind by a
/// `write()` that crashed before its rename) that are at least `ttl` old, throttled to at most
/// once per `SWEEP_INTERVAL` via a marker file. This is the only place a `.worker` file is ever
/// removed other than `clear()`'s single-path unlink on an unclean loss: a clean session end
/// deliberately leaves its own pin alone (a later seek/track-change restart must still find it),
/// so without a sweep the file has no owner left to delete it once the session truly ends and is
/// never touched again -- `read()`'s TTL check only makes such a file stop being *used*, it never
/// unlinks it (see docs/PLAN.md §10). Every PLAYBACK output shares one scratch directory, so
/// calling this from any active session's `run()` also reaps every other session's leftover
/// files in the same directory, not just its own. Best-effort: every I/O error here is swallowed,
/// same as the rest of this module -- affinity is an optimization, cleanup is a nice-to-have on
/// top of it, and neither is a correctness requirement.
pub fn sweep_stale(dir: &Path, ttl: Duration) {
    let marker = dir.join(".affinity-sweep");
    if let Some(modified) = fs::metadata(&marker).ok().and_then(|m| m.modified().ok()) {
        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or(Duration::ZERO);
        if age < SWEEP_INTERVAL {
            return;
        }
    }
    // Touch the marker before the readdir below (even if that readdir then fails) so a
    // persistently-unreadable directory is retried at most once per interval, not on every call.
    let _ = fs::write(&marker, b"");
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.ends_with(".worker") || name.contains(".worker.")) {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or(Duration::ZERO);
        if age >= ttl {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempScratch(PathBuf);
    impl TempScratch {
        fn new(tag: &str) -> TempScratch {
            let dir = std::env::temp_dir().join(format!(
                "tcpool-affinity-test-{tag}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&dir).expect("create scratch dir");
            TempScratch(dir)
        }
        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
    }
    impl Drop for TempScratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn affinity_path_sits_next_to_the_playlist_as_the_md5_stem_plus_worker() {
        assert_eq!(
            affinity_path("/scratch/abc123.m3u8"),
            PathBuf::from("/scratch/abc123.worker")
        );
    }

    #[test]
    fn read_is_none_when_the_file_is_missing() {
        let t = TempScratch::new("missing");
        assert_eq!(read(&t.path("abc.worker"), DEFAULT_TTL), None);
    }

    #[test]
    fn write_then_read_round_trips_the_worker_name() {
        let t = TempScratch::new("roundtrip");
        let path = t.path("abc.worker");
        write(&path, "p4-nvenc");
        assert_eq!(read(&path, DEFAULT_TTL), Some("p4-nvenc".to_string()));
        assert!(
            fs::read_dir(&t.0)
                .unwrap()
                .flatten()
                .all(|e| !e.file_name().to_string_lossy().ends_with(".tmp")),
            "no leftover tmp file after a successful write"
        );
    }

    #[test]
    fn a_second_write_overwrites_the_pin() {
        let t = TempScratch::new("overwrite");
        let path = t.path("abc.worker");
        write(&path, "arc-qsv");
        write(&path, "p4-nvenc");
        assert_eq!(read(&path, DEFAULT_TTL), Some("p4-nvenc".to_string()));
    }

    #[test]
    fn clear_removes_the_file() {
        let t = TempScratch::new("clear");
        let path = t.path("abc.worker");
        write(&path, "arc-qsv");
        clear(&path);
        assert_eq!(read(&path, DEFAULT_TTL), None);
    }

    #[test]
    fn clear_of_a_missing_file_does_not_panic() {
        let t = TempScratch::new("clear-missing");
        clear(&t.path("nope.worker")); // must not panic
    }

    #[test]
    fn read_is_none_once_older_than_the_ttl() {
        let t = TempScratch::new("stale");
        let path = t.path("abc.worker");
        write(&path, "arc-qsv");
        let f = File::options().write(true).open(&path).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(10))
            .unwrap();
        assert_eq!(
            read(&path, Duration::from_secs(5)),
            None,
            "10s old must be stale against a 5s TTL"
        );
        assert_eq!(
            read(&path, Duration::from_secs(20)),
            Some("arc-qsv".to_string()),
            "10s old must still be fresh against a 20s TTL"
        );
    }

    #[test]
    fn read_tolerates_garbage_content() {
        let t = TempScratch::new("garbage");
        let path = t.path("abc.worker");
        fs::write(&path, [0xff, 0xfe, 0x00, 0xff]).unwrap(); // not valid UTF-8
        assert_eq!(read(&path, DEFAULT_TTL), None);
    }

    #[test]
    fn read_tolerates_a_whitespace_only_file() {
        let t = TempScratch::new("blank");
        let path = t.path("abc.worker");
        fs::write(&path, "   \n").unwrap();
        assert_eq!(read(&path, DEFAULT_TTL), None);
    }

    #[test]
    fn enabled_reads_its_own_var_and_defaults_on() {
        let var = "TCPOOL_TEST_AFFINITY_ENABLED";
        std::env::remove_var(var);
        assert!(enabled_var(var), "unset must default to enabled");
        std::env::set_var(var, "0");
        assert!(!enabled_var(var), "\"0\" must disable");
        std::env::set_var(var, "1");
        assert!(enabled_var(var), "any other value must stay enabled");
        std::env::remove_var(var);
    }

    #[test]
    fn sweep_stale_removes_a_worker_file_past_ttl_and_keeps_a_fresh_one() {
        let t = TempScratch::new("sweep");
        let stale = t.path("stale.worker");
        let fresh = t.path("fresh.worker");
        write(&stale, "arc-qsv");
        write(&fresh, "p4-nvenc");
        let f = File::options().write(true).open(&stale).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(10))
            .unwrap();
        sweep_stale(&t.0, Duration::from_secs(5));
        assert_eq!(read(&stale, DEFAULT_TTL), None, "stale file must be gone");
        assert_eq!(
            read(&fresh, DEFAULT_TTL),
            Some("p4-nvenc".to_string()),
            "fresh file must survive the sweep"
        );
    }

    #[test]
    fn sweep_stale_leaves_a_leftover_tmp_file_alone_when_young_but_removes_it_once_old() {
        let t = TempScratch::new("sweep-tmp");
        // Mirror write()'s own tmp naming: "<file>.<pid>.tmp".
        let tmp = t.path("abc.worker.999.tmp");
        fs::write(&tmp, "p4-nvenc").unwrap();
        sweep_stale(&t.0, Duration::from_secs(5));
        assert!(tmp.exists(), "a young leftover tmp file must survive");
        let f = File::options().write(true).open(&tmp).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(10))
            .unwrap();
        // The call above already wrote this dir's throttle marker; drop it so this second call
        // walks the directory again instead of being (correctly, but not what this test checks)
        // throttled -- that behavior has its own test below.
        let _ = fs::remove_file(t.path(".affinity-sweep"));
        sweep_stale(&t.0, Duration::from_secs(5));
        assert!(!tmp.exists(), "an old leftover tmp file must be removed");
    }

    #[test]
    fn sweep_stale_is_throttled_by_its_own_marker_within_the_interval() {
        let t = TempScratch::new("sweep-throttle");
        let old = t.path("old.worker");
        write(&old, "arc-qsv");
        let f = File::options().write(true).open(&old).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(10))
            .unwrap();
        sweep_stale(&t.0, Duration::from_secs(5)); // first call: walks, removes `old`, writes marker
        assert!(!old.exists());
        // A second stale file planted right after: the marker is fresh, so this call must be a
        // no-op (a bare `stat` on the marker, no readdir) and leave it in place.
        let second = t.path("second.worker");
        write(&second, "p4-nvenc");
        let f = File::options().write(true).open(&second).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(10))
            .unwrap();
        sweep_stale(&t.0, Duration::from_secs(5));
        assert!(
            second.exists(),
            "throttled: a second sweep inside SWEEP_INTERVAL must not walk the directory again"
        );
    }

    #[test]
    fn ttl_from_env_falls_back_on_unset_or_unparsable() {
        let var = "TCPOOL_TEST_AFFINITY_TTL";
        std::env::remove_var(var);
        assert_eq!(ttl_var(var), DEFAULT_TTL);
        std::env::set_var(var, "not-a-number");
        assert_eq!(ttl_var(var), DEFAULT_TTL);
        std::env::set_var(var, "30");
        assert_eq!(ttl_var(var), Duration::from_secs(30));
        std::env::remove_var(var);
    }
}
