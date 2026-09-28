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
//!
//! Next to it, `<keepalive>.seg` (bughunt patch 16): the index of the last segment any replica
//! served a request for; mtime = when. A detached job reads it to stay a bounded distance ahead
//! of its viewer (see [`orphan_throttle`]); nothing else depends on it.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Env var Jellyfin (bughunt patch 16, shared mode) sets on every HLS ffmpeg it starts.
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

/// `<keepalive>.seg`: the viewer's position sidecar (see the module docs).
pub fn position_path(keepalive: &Path) -> PathBuf {
    let mut p = keepalive.as_os_str().to_owned();
    p.push(".seg");
    PathBuf::from(p)
}

/// The last segment index a replica served a request for, and when (the file's mtime).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub index: u64,
    pub touched: SystemTime,
}

/// Reads the position sidecar. Missing, unreadable, empty or garbled (a torn write) -> `None`.
pub fn read_position(path: &Path) -> Option<Position> {
    let touched = std::fs::metadata(path).ok()?.modified().ok()?;
    let index = std::fs::read_to_string(path).ok()?.trim().parse().ok()?;
    Some(Position { index, touched })
}

/// The segment index in one line of ffmpeg's stderr announcing that the HLS muxer opens a
/// segment of this output: `[hls @ 0x..] Opening '<dir>/<stem><index>.ts.tmp' for writing`
/// (info level, printed for every segment; `.tmp` with `-hls_flags temp_file`).
///
/// This is how the agent knows how far its own ffmpeg has got. The playlist cannot tell: with
/// Jellyfin's `-hls_playlist_type vod` ffmpeg writes it only at the very end (MEASURED, ffmpeg
/// 8.1). Neither can the directory: an earlier writer's segments of the same output may still
/// be there. Opening segment N means segments up to N-1 are complete.
pub fn opened_segment(line: &str, stem: &str) -> Option<u64> {
    let rest = &line[line.find("Opening '")? + "Opening '".len()..];
    let path = &rest[..rest.find("' for writing")?];
    let name = path.rsplit('/').next()?;
    let tail = name.strip_prefix(stem)?;
    let end = tail.find('.')?;
    let num = &tail[..end];
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    num.parse().ok()
}

/// The HLS segment length in seconds (`-hls_time`), if the argv sets a positive one.
pub fn segment_seconds(args: &[String]) -> Option<f64> {
    let i = args.iter().position(|a| a == "-hls_time")?;
    let v: f64 = args.get(i + 1)?.parse().ok()?;
    (v.is_finite() && v > 0.0).then_some(v)
}

/// What a detached job's watcher should do with its ffmpeg this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Throttle {
    /// Send `p`: the output is more than `lead_max` ahead of the viewer.
    Pause,
    /// Send `u`: the viewer is within `lead_resume` of the output, or its position is unknown
    /// (then a detached job runs unthrottled, as it did before the position sidecar existed).
    Resume,
    /// Keep the current state (between the two thresholds: hysteresis).
    Leave,
}

/// Limits for [`orphan_throttle`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThrottleLimits {
    /// Pause when the output leads the viewer by more than this. Zero disables throttling.
    pub lead_max: Duration,
    /// Resume when the lead drops below this.
    pub lead_resume: Duration,
    /// A position older than this, while the keepalive says `playing`, is unknown.
    pub pos_stale: Duration,
}

/// The viewer's position if it can be trusted: fresh (within `pos_stale`), or any age while the
/// keepalive says the client is paused (a paused viewer requests nothing).
pub fn usable_position(
    now: SystemTime,
    pos: Option<Position>,
    ka: Option<Keepalive>,
    pos_stale: Duration,
) -> Option<Position> {
    let pos = pos?;
    let client_paused = ka.is_some_and(|k| k.paused);
    (client_paused || now.duration_since(pos.touched).unwrap_or_default() <= pos_stale)
        .then_some(pos)
}

/// The throttling decision for a detached job (Jellyfin's own throttler did this while the job
/// had an owner): keep the output at most `lead_max` ahead of the last segment the viewer asked
/// for, with hysteresis down to `lead_resume`.
///
/// `paused` is whether ffmpeg is paused now. `newest` is the newest segment written, `pos` the
/// viewer's last requested segment, `seg_secs` the segment length. A paused client (keepalive
/// `paused`) keeps its last position however old it is, so a job stays paused under a paused
/// viewer. If the position is unknown (no sidecar, no segment length, no segment written yet, or a
/// `playing` client whose position went stale, see [`usable_position`]) the job runs
/// unthrottled, the behaviour before the sidecar existed.
pub fn orphan_throttle(
    now: SystemTime,
    paused: bool,
    newest: Option<u64>,
    pos: Option<Position>,
    ka: Option<Keepalive>,
    seg_secs: Option<f64>,
    lim: ThrottleLimits,
) -> Throttle {
    let unknown = if paused {
        Throttle::Resume
    } else {
        Throttle::Leave
    };
    if lim.lead_max.is_zero() {
        return unknown;
    }
    let pos = usable_position(now, pos, ka, lim.pos_stale);
    let (Some(newest), Some(pos), Some(seg)) = (newest, pos, seg_secs) else {
        return unknown;
    };
    let lead = newest.saturating_sub(pos.index) as f64 * seg;
    if !paused && lead > lim.lead_max.as_secs_f64() {
        Throttle::Pause
    } else if paused && lead < lim.lead_resume.as_secs_f64() {
        Throttle::Resume
    } else {
        Throttle::Leave
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lim() -> ThrottleLimits {
        ThrottleLimits {
            lead_max: Duration::from_secs(60),
            lead_resume: Duration::from_secs(30),
            pos_stale: Duration::from_secs(60),
        }
    }

    #[test]
    fn throttle_hysteresis() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let pos = |i| {
            Some(Position {
                index: i,
                touched: t0,
            })
        };
        let playing = Some(Keepalive {
            touched: t0,
            paused: false,
        });
        let th = |paused, newest, p| {
            orphan_throttle(t0, paused, Some(newest), p, playing, Some(3.0), lim())
        };
        // 3 s segments: 60 s = 20 segments, 30 s = 10
        assert_eq!(th(false, 110, pos(100)), Throttle::Leave);
        assert_eq!(
            th(false, 120, pos(100)),
            Throttle::Leave,
            "exactly at the limit"
        );
        assert_eq!(th(false, 121, pos(100)), Throttle::Pause);
        assert_eq!(th(true, 121, pos(100)), Throttle::Leave, "already paused");
        assert_eq!(
            th(true, 111, pos(100)),
            Throttle::Leave,
            "between the thresholds"
        );
        assert_eq!(th(true, 109, pos(100)), Throttle::Resume);
        assert_eq!(th(false, 109, pos(100)), Throttle::Leave, "already running");
        // the viewer is past the output (it waits for a segment): never pause
        assert_eq!(th(false, 90, pos(100)), Throttle::Leave);
        assert_eq!(th(true, 90, pos(100)), Throttle::Resume);
        // a seek back inside the written range: pause again
        assert_eq!(th(false, 300, pos(10)), Throttle::Pause);
        // disabled
        let off = ThrottleLimits {
            lead_max: Duration::ZERO,
            ..lim()
        };
        assert_eq!(
            orphan_throttle(t0, true, Some(500), pos(0), playing, Some(3.0), off),
            Throttle::Resume
        );
        assert_eq!(
            orphan_throttle(t0, false, Some(500), pos(0), playing, Some(3.0), off),
            Throttle::Leave
        );
    }

    #[test]
    fn throttle_unknown_position_runs_unthrottled() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let s = Duration::from_secs;
        let playing = Some(Keepalive {
            touched: t0,
            paused: false,
        });
        let paused_ka = Some(Keepalive {
            touched: t0,
            paused: true,
        });
        let fresh = Some(Position {
            index: 0,
            touched: t0,
        });
        let old = Some(Position {
            index: 0,
            touched: t0 - s(61),
        });
        for (newest, pos, seg) in [
            (None, fresh, Some(3.0)),
            (Some(500), None, Some(3.0)),
            (Some(500), fresh, None),
        ] {
            assert_eq!(
                orphan_throttle(t0, true, newest, pos, playing, seg, lim()),
                Throttle::Resume
            );
            assert_eq!(
                orphan_throttle(t0, false, newest, pos, playing, seg, lim()),
                Throttle::Leave
            );
        }
        // stale while playing (or no keepalive at all) -> unknown -> resume
        assert_eq!(
            orphan_throttle(t0, true, Some(500), old, playing, Some(3.0), lim()),
            Throttle::Resume
        );
        assert_eq!(
            orphan_throttle(t0, true, Some(500), old, None, Some(3.0), lim()),
            Throttle::Resume
        );
        // a paused client keeps its (old) position: stay paused / pause
        assert_eq!(
            orphan_throttle(t0, true, Some(500), old, paused_ka, Some(3.0), lim()),
            Throttle::Leave
        );
        assert_eq!(
            orphan_throttle(t0, false, Some(500), old, paused_ka, Some(3.0), lim()),
            Throttle::Pause
        );
        // a position "from the future" (clock skew) is fresh
        let fut = Some(Position {
            index: 0,
            touched: t0 + s(100),
        });
        assert_eq!(
            orphan_throttle(t0, false, Some(500), fut, playing, Some(3.0), lim()),
            Throttle::Pause
        );
    }

    #[test]
    fn position_file() {
        let d = std::env::temp_dir().join(format!("tcpool-pos-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let ka = d.join("sess");
        let sp = position_path(&ka);
        assert_eq!(sp, d.join("sess.seg"));
        assert!(read_position(&sp).is_none());
        std::fs::write(&sp, "42\n").unwrap();
        assert_eq!(read_position(&sp).unwrap().index, 42);
        std::fs::write(&sp, "").unwrap();
        assert!(read_position(&sp).is_none(), "a torn write is unknown");
        std::fs::write(&sp, "-3").unwrap();
        assert!(read_position(&sp).is_none());

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn opened_segment_lines() {
        let stem = "0123456789abcdef0123456789abcdef";
        let l = |s: &str| format!("[hls @ 0x55da9b417c40] Opening '{s}' for writing");
        assert_eq!(
            opened_segment(&l(&format!("/transcodes/jf/{stem}12.ts.tmp")), stem),
            Some(12)
        );
        assert_eq!(
            opened_segment(&l(&format!("/transcodes/jf/{stem}0.ts")), stem),
            Some(0)
        );
        assert_eq!(
            opened_segment(&l(&format!("/t/{stem}7.mp4")), stem),
            Some(7)
        );
        // the playlist itself, an fMP4 init segment, another output, noise: none
        assert!(opened_segment(&l(&format!("/t/{stem}.m3u8.tmp")), stem).is_none());
        assert!(opened_segment(&l(&format!("/t/{stem}-1.mp4")), stem).is_none());
        assert!(opened_segment(&l("/t/ffffffffffffffffffffffffffffffff3.ts.tmp"), stem).is_none());
        assert!(opened_segment("frame= 100 fps=50 time=00:00:04.00 speed=2x", stem).is_none());
        assert!(opened_segment(&format!("Opening '/t/{stem}3.ts.tmp"), stem).is_none());
    }

    #[test]
    fn hls_time() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(segment_seconds(&a(&["-hls_time", "3"])), Some(3.0));
        assert_eq!(segment_seconds(&a(&["-hls_time", "6.5", "x"])), Some(6.5));
        assert_eq!(segment_seconds(&a(&["-hls_time", "0"])), None);
        assert_eq!(segment_seconds(&a(&["-hls_time"])), None);
        assert_eq!(segment_seconds(&a(&["-f", "hls"])), None);
    }

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
