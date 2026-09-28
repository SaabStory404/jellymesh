//! The shared-transcode-directory file contract (transcode/docs/SHARED-TRANSCODE.md).
//!
//! Several Jellyfin replicas write one transcode directory. Next to each HLS playlist
//! `<dir>/<stem>.m3u8` live these siblings, all keyed by the same `<stem>` (Jellyfin's
//! `MD5(MediaPath-UserAgent-DeviceId-PlaySessionId)`, 32 hex chars):
//!
//! | file | written by | meaning |
//! |---|---|---|
//! | `<stem>.tcpool.lock` | the shim that won the lease (`O_EXCL`); heartbeated by that shim **and** by the agent running the job | one encoder per output; content = holder token |
//! | `<stem>.tcpool.takeover` | a shim on another replica that must restart this output (a seek) | content = the holder token it wants gone; the agent running under that token ends its ffmpeg and frees the lock |
//! | `<stem>.worker` | the shim (seek affinity) | unchanged, see the shim's `affinity` module |
//!
//! Plus one file per playback session, outside the per-output namespace because progress pings
//! carry only the PlaySessionId: `<transcode dir>/.jellymesh-alive/<PlaySessionId>`, touched by
//! whichever Jellyfin replica receives a segment request or a progress ping for that session.
//! Content: `paused` or `playing`. Jellyfin passes its path to ffmpeg (the shim) in the
//! `JELLYMESH_KEEPALIVE` environment variable, and the shim forwards it in `Job.keepalive_path`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Env var Jellyfin (bughunt patch 14, shared mode) sets on every HLS ffmpeg it starts.
pub const KEEPALIVE_ENV: &str = "JELLYMESH_KEEPALIVE";

/// `<dir>/<stem>.tcpool.lock` for a playlist `<dir>/<stem>.m3u8`.
pub fn lease_path(playlist: &str) -> PathBuf {
    sibling(playlist, "tcpool.lock")
}

/// `<dir>/<stem>.tcpool.takeover` for a playlist `<dir>/<stem>.m3u8`.
pub fn takeover_path(playlist: &str) -> PathBuf {
    sibling(playlist, "tcpool.takeover")
}

fn sibling(playlist: &str, ext: &str) -> PathBuf {
    let p = Path::new(playlist);
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    p.with_file_name(format!("{stem}.{ext}"))
}

/// The playlist's `(dir, stem)` when the stem is safe to use as a delete prefix: at least 16
/// characters, ASCII hex only. Jellyfin's stems are 32-char MD5 hex; anything else (an empty or
/// short stem would turn "delete `<stem>*`" into "delete the directory") is refused.
pub fn output_stem(playlist: &str) -> Option<(PathBuf, String)> {
    let p = Path::new(playlist);
    let dir = p.parent()?.to_path_buf();
    let stem = p.file_stem()?.to_str()?.to_string();
    (stem.len() >= 16 && stem.bytes().all(|b| b.is_ascii_hexdigit())).then_some((dir, stem))
}

/// Whether `path` is an absolute path strictly under `root`, with no `..`/`.` components.
pub fn under_root(path: &str, root: &str) -> bool {
    // Checked on the raw string: `Path::components` silently drops interior `.` segments.
    if !path.starts_with('/') || path.split('/').any(|seg| seg == "." || seg == "..") {
        return false;
    }
    let root = root.trim_end_matches('/');
    !root.is_empty() && path.len() > root.len() + 1 && path.starts_with(&format!("{root}/"))
}

/// What a keepalive file says: when it was last touched and whether the client reported paused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Keepalive {
    pub touched: SystemTime,
    pub paused: bool,
}

pub fn read_keepalive(path: &Path) -> Option<Keepalive> {
    let touched = std::fs::metadata(path).ok()?.modified().ok()?;
    let paused = std::fs::read(path)
        .map(|b| b.starts_with(b"paused"))
        .unwrap_or(false);
    Some(Keepalive { touched, paused })
}

/// Whether a detached (orphaned) job's viewer is gone: nothing touched the keepalive for longer
/// than the idle limit (or the paused limit, when the last touch said `paused`). `since` is when
/// the job detached: a missing or older keepalive counts from then, so a job never expires the
/// instant it detaches.
pub fn orphan_expired(
    now: SystemTime,
    since: SystemTime,
    ka: Option<Keepalive>,
    idle: Duration,
    paused: Duration,
) -> bool {
    let (last, limit) = match ka {
        Some(k) => (k.touched.max(since), if k.paused { paused } else { idle }),
        None => (since, idle),
    };
    now.duration_since(last).unwrap_or_default() > limit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn siblings() {
        let pl = "/transcodes/jf/0123456789abcdef0123456789abcdef.m3u8";
        assert_eq!(
            lease_path(pl),
            PathBuf::from("/transcodes/jf/0123456789abcdef0123456789abcdef.tcpool.lock")
        );
        assert_eq!(
            takeover_path(pl),
            PathBuf::from("/transcodes/jf/0123456789abcdef0123456789abcdef.tcpool.takeover")
        );
    }

    #[test]
    fn stem_must_be_long_hex() {
        assert!(output_stem("/t/0123456789abcdef0123456789abcdef.m3u8").is_some());
        assert!(output_stem("/t/p.m3u8").is_none());
        assert!(output_stem("/t/.m3u8").is_none());
        assert!(output_stem("/t/0123456789abcdefXYZ3456789abcdef.m3u8").is_none());
    }

    #[test]
    fn under_root_rules() {
        assert!(under_root(
            "/transcodes/jf/.jellymesh-alive/abc",
            "/transcodes"
        ));
        assert!(under_root("/transcodes/x", "/transcodes/"));
        assert!(!under_root("/transcodes", "/transcodes"));
        assert!(!under_root("/transcodes/", "/transcodes"));
        assert!(!under_root("/transcodesX/a", "/transcodes"));
        assert!(!under_root("/transcodes/../etc/passwd", "/transcodes"));
        assert!(!under_root("/transcodes/./a", "/transcodes"));
        assert!(!under_root("transcodes/a", "/transcodes"));
        assert!(!under_root("/etc/a", ""));
    }

    #[test]
    fn orphan_expiry() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let idle = Duration::from_secs(60);
        let paused = Duration::from_secs(180);
        let s = Duration::from_secs;
        // no keepalive at all: counts from the detach
        assert!(!orphan_expired(t0 + s(59), t0, None, idle, paused));
        assert!(orphan_expired(t0 + s(61), t0, None, idle, paused));
        // an old keepalive does not expire the job at detach time
        let old = Keepalive {
            touched: t0 - s(500),
            paused: false,
        };
        assert!(!orphan_expired(t0 + s(1), t0, Some(old), idle, paused));
        // fresh touches keep it alive
        let fresh = Keepalive {
            touched: t0 + s(100),
            paused: false,
        };
        assert!(!orphan_expired(t0 + s(150), t0, Some(fresh), idle, paused));
        assert!(orphan_expired(t0 + s(161), t0, Some(fresh), idle, paused));
        // paused gets the longer grace
        let p = Keepalive {
            touched: t0 + s(100),
            paused: true,
        };
        assert!(!orphan_expired(t0 + s(270), t0, Some(p), idle, paused));
        assert!(orphan_expired(t0 + s(281), t0, Some(p), idle, paused));
        // clock skew (a touch in the future) never expires
        let fut = Keepalive {
            touched: t0 + s(10_000),
            paused: false,
        };
        assert!(!orphan_expired(t0 + s(5), t0, Some(fut), idle, paused));
    }

    #[test]
    fn keepalive_file() {
        let d = std::env::temp_dir().join(format!("tcpool-ka-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("sess");
        assert!(read_keepalive(&f).is_none());
        std::fs::write(&f, "paused").unwrap();
        assert!(read_keepalive(&f).unwrap().paused);
        std::fs::write(&f, "playing").unwrap();
        assert!(!read_keepalive(&f).unwrap().paused);
        let _ = std::fs::remove_dir_all(&d);
    }
}
