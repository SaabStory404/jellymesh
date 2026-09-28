//! Per-output lease: one encoder per HLS output prefix, across Jellyfin replicas.
//!
//! With two Jellyfin replicas sharing the scratch and no session stickiness, replica B has no
//! `TranscodingJob` for A's session, so on a request past A's edge it starts a second ffmpeg on
//! the same `<md5>N.ts` names while A's is still writing. The lease is an `O_EXCL` file next to
//! the playlist whose mtime is heartbeated; the loser follows the holder (serving its segments
//! from disk) and takes over only when the holder's lease goes stale.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// A lease older than this is dead (holder heartbeats every second; NFS attributes cache 1 s).
pub const STALE_AFTER: Duration = Duration::from_secs(6);

pub struct Lease {
    path: PathBuf,
    token: String,
    stop: Arc<AtomicBool>,
}

pub enum Acquire {
    Held(Lease),
    /// Someone else holds a fresh lease.
    Busy,
    /// Lease dir not usable (e.g. scratch not mounted): run unleased, as the spike did.
    Unavailable(std::io::Error),
}

pub fn lease_path(playlist: &str) -> PathBuf {
    tcpool_ir::shared::lease_path(playlist)
}

fn age(path: &Path) -> Option<Duration> {
    let m = fs::metadata(path).ok()?.modified().ok()?;
    Some(SystemTime::now().duration_since(m).unwrap_or_default())
}

pub fn is_fresh(path: &Path) -> bool {
    age(path).is_some_and(|a| a < STALE_AFTER)
}

pub fn try_acquire(playlist: &str) -> Acquire {
    let path = lease_path(playlist);
    let token = format!(
        "{}:{}:{}",
        std::env::var("HOSTNAME").unwrap_or_default(),
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    for attempt in 0..2 {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                let _ = f.write_all(token.as_bytes());
                let lease = Lease {
                    path,
                    token,
                    stop: Arc::new(AtomicBool::new(false)),
                };
                lease.start_heartbeat();
                return Acquire::Held(lease);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if attempt == 0 && !is_fresh(&path) {
                    let _ = fs::remove_file(&path); // stale: the holder is gone
                    continue;
                }
                return Acquire::Busy;
            }
            Err(e) => return Acquire::Unavailable(e),
        }
    }
    Acquire::Busy
}

impl Lease {
    fn start_heartbeat(&self) {
        let path = self.path.clone();
        let stop = self.stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_secs(1));
                if let Ok(f) = File::options().write(true).open(&path) {
                    let _ = f.set_modified(SystemTime::now());
                }
            }
        });
    }

    pub fn release(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let mut s = String::new();
        if File::open(&self.path)
            .and_then(|mut f| f.read_to_string(&mut s))
            .is_ok()
            && s == self.token
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Shared transcode dir: ask the current holder's agent to stop (a `<stem>.tcpool.takeover` file
/// naming the holder's token, written atomically), then acquire once the lease is freed. `None`
/// when the holder did not let go within `wait` (e.g. an agent without takeover support) or the
/// lease dir is unusable; the caller then follows as before.
pub async fn take_over(playlist: &str, wait: Duration) -> Option<Lease> {
    let lock = lease_path(playlist);
    let req = tcpool_ir::shared::takeover_path(playlist);
    let holder = match fs::read_to_string(&lock) {
        Ok(h) => h,
        // Gone between our O_EXCL attempt and now: just try again.
        Err(_) => {
            return match try_acquire(playlist) {
                Acquire::Held(l) => Some(l),
                _ => None,
            }
        }
    };
    let tmp = PathBuf::from(format!("{}.{}.tmp", req.display(), std::process::id()));
    if fs::write(&tmp, holder.as_bytes()).is_err() || fs::rename(&tmp, &req).is_err() {
        let _ = fs::remove_file(&tmp);
        return None;
    }
    let deadline = std::time::Instant::now() + wait;
    let got = loop {
        match try_acquire(playlist) {
            Acquire::Held(l) => break Some(l),
            Acquire::Unavailable(_) => break None,
            Acquire::Busy if std::time::Instant::now() >= deadline => break None,
            Acquire::Busy => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    };
    // Withdraw the request only if it is still ours (a later takeover may have replaced it).
    if fs::read_to_string(&req).is_ok_and(|t| t == holder) {
        let _ = fs::remove_file(&req);
    }
    got
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_then_released() {
        let dir = std::env::temp_dir().join(format!("tcpool-lease-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let pl = dir.join("abc.m3u8").to_string_lossy().into_owned();
        let a = match try_acquire(&pl) {
            Acquire::Held(l) => l,
            _ => panic!("first acquire"),
        };
        assert!(matches!(try_acquire(&pl), Acquire::Busy));
        a.release();
        assert!(matches!(try_acquire(&pl), Acquire::Held(_)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn take_over_waits_for_the_holder_to_let_go() {
        let dir = std::env::temp_dir().join(format!("tcpool-takeover-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let pl = dir.join("0123456789abcdef0123456789abcdef.m3u8");
        let pl = pl.to_string_lossy().into_owned();
        let a = match try_acquire(&pl) {
            Acquire::Held(l) => l,
            _ => panic!("first acquire"),
        };
        let req = tcpool_ir::shared::takeover_path(&pl);
        // A fake agent: when the request names the holder's token, free the lease.
        let (req2, tok) = (req.clone(), a.token.clone());
        let holder = std::thread::spawn(move || {
            for _ in 0..50 {
                if fs::read_to_string(&req2).is_ok_and(|t| t == tok) {
                    a.release();
                    return true;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            false
        });
        let b = take_over(&pl, Duration::from_secs(5)).await;
        assert!(holder.join().unwrap(), "holder saw the request");
        assert!(b.is_some(), "took the lease over");
        assert!(!req.exists(), "request withdrawn after success");
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn take_over_gives_up_on_a_holder_that_never_answers() {
        let dir = std::env::temp_dir().join(format!("tcpool-takeover2-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let pl = dir.join("0123456789abcdef0123456789abcdef.m3u8");
        let pl = pl.to_string_lossy().into_owned();
        let _a = match try_acquire(&pl) {
            Acquire::Held(l) => l,
            _ => panic!("first acquire"),
        };
        let t0 = std::time::Instant::now();
        assert!(take_over(&pl, Duration::from_millis(600)).await.is_none());
        assert!(t0.elapsed() >= Duration::from_millis(600));
        assert!(!tcpool_ir::shared::takeover_path(&pl).exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
