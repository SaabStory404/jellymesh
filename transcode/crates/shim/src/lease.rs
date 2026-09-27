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
    let p = Path::new(playlist);
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    p.with_file_name(format!("{stem}.tcpool.lock"))
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
}
