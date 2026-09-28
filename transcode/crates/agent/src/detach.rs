//! Shared transcode dir: a PLAYBACK job that outlives its shim (transcode/docs/SHARED-TRANSCODE.md).
//!
//! When every Jellyfin replica serves one transcode directory, the replica that started a job can
//! die (pod delete, node loss, kill -9) while the viewer keeps playing through another replica,
//! which serves the segments this job is still writing. Fencing the job on shim loss (the
//! original contract) would stop the encode and force a restart on the new replica. So a job
//! whose shim sent a `keepalive_path` instead *detaches*: ffmpeg keeps running, the agent
//! heartbeats the output lease itself (it did so from the start, so the lease never went stale),
//! and the job ends when
//! - the session keepalive goes stale (no replica saw a segment request or progress ping for
//!   `orphan_idle`, or `orphan_paused` after a paused ping) -> ffmpeg killed, outputs deleted;
//! - another replica's shim asks to take the output over (`<stem>.tcpool.takeover` naming our
//!   lease token) -> ffmpeg killed, lease freed, outputs left to the new writer;
//! - ffmpeg finishes on its own -> lease freed; the outputs stay (they are being served) until
//!   the keepalive goes stale, then deleted.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use tcpool_ir::shared;

use crate::config::Config;

/// Everything the agent needs to run one job detachably. Built only for PLAYBACK HLS jobs whose
/// shim sent a valid keepalive path and whose lease file exists (i.e. the shim holds the lease).
#[derive(Debug)]
pub struct Detach {
    pub lease: PathBuf,
    pub takeover: PathBuf,
    pub keepalive: PathBuf,
    pub dir: PathBuf,
    pub stem: String,
    /// The lease holder token read at job start: the lease is "ours" while it still says this.
    pub token: String,
    detached_at: Mutex<Option<SystemTime>>,
    taken_over: AtomicBool,
}

impl Detach {
    /// `args` are the worker-side (path-mapped) argv; `keepalive` is already path-mapped too.
    pub fn from_job(
        cfg: &Config,
        args: &[String],
        keepalive: &str,
    ) -> Result<Option<Detach>, String> {
        if !cfg.detach || keepalive.is_empty() {
            return Ok(None);
        }
        if !shared::under_root(keepalive, &cfg.policy.output_root) {
            return Err(format!(
                "keepalive path not under the output root: {keepalive}"
            ));
        }
        let Some(pl) = tcpool_ir::playlist(args) else {
            return Ok(None);
        };
        let Some((dir, stem)) = shared::output_stem(pl) else {
            return Ok(None); // not a Jellyfin-shaped output: no safe delete prefix, run attached
        };
        let lease = shared::lease_path(pl);
        let Ok(token) = fs::read_to_string(&lease) else {
            return Ok(None); // shim ran unleased: a second writer can't be excluded, run attached
        };
        Ok(Some(Detach {
            takeover: shared::takeover_path(pl),
            lease,
            keepalive: PathBuf::from(keepalive),
            dir,
            stem,
            token,
            detached_at: Mutex::new(None),
            taken_over: AtomicBool::new(false),
        }))
    }

    /// Mark detached; true only on the first call.
    pub fn mark_detached(&self) -> bool {
        let mut d = self.detached_at.lock().unwrap_or_else(|p| p.into_inner());
        if d.is_some() {
            return false;
        }
        *d = Some(SystemTime::now());
        true
    }

    pub fn detached_at(&self) -> Option<SystemTime> {
        *self.detached_at.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn is_detached(&self) -> bool {
        self.detached_at().is_some()
    }

    pub fn set_taken_over(&self) {
        self.taken_over.store(true, Ordering::SeqCst);
    }

    pub fn was_taken_over(&self) -> bool {
        self.taken_over.load(Ordering::SeqCst)
    }

    /// Keep the lease fresh (open without create: a lease someone else removed stays removed).
    pub fn heartbeat(&self) {
        if lease_is_ours(&self.lease, &self.token) {
            if let Ok(f) = fs::File::options().write(true).open(&self.lease) {
                let _ = f.set_modified(SystemTime::now());
            }
        }
    }

    /// Another replica's shim asked for this output, naming our token.
    pub fn takeover_requested(&self) -> bool {
        fs::read_to_string(&self.takeover).is_ok_and(|t| t == self.token)
    }

    /// Whether the detached job's viewer is gone.
    pub fn expired(&self, cfg: &Config, now: SystemTime) -> bool {
        let Some(since) = self.detached_at() else {
            return false;
        };
        now.duration_since(since).unwrap_or_default() > cfg.orphan_max
            || shared::orphan_expired(
                now,
                since,
                shared::read_keepalive(&self.keepalive),
                cfg.orphan_idle,
                cfg.orphan_paused,
            )
    }

    /// Free the lease if it is still ours.
    pub fn release(&self) {
        if lease_is_ours(&self.lease, &self.token) {
            let _ = fs::remove_file(&self.lease);
        }
    }

    /// Delete `<stem>*` in the output dir (playlist, segments, partials, lease, affinity) and the
    /// session keepalive, unless someone else holds a lease on the output now.
    pub fn cleanup(&self) -> usize {
        if fs::read_to_string(&self.lease).is_ok_and(|t| t != self.token) {
            return 0; // another writer owns this output now
        }
        let n = delete_prefix(&self.dir, &self.stem);
        let _ = fs::remove_file(&self.keepalive);
        n
    }
}

fn lease_is_ours(lease: &Path, token: &str) -> bool {
    fs::read_to_string(lease).is_ok_and(|t| t == token)
}

/// Remove every file directly in `dir` whose name starts with `stem` (callers guarantee `stem`
/// is a long hex string, see `shared::output_stem`). Returns how many were removed.
pub fn delete_prefix(dir: &Path, stem: &str) -> usize {
    if stem.len() < 16 {
        return 0;
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return 0;
    };
    rd.flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(stem))
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter(|e| fs::remove_file(e.path()).is_ok())
        .count()
}

/// After a detached job's ffmpeg exited on its own: the outputs are still being served, so wait
/// for the viewer to go (keepalive stale) before deleting them. Bounded by `orphan_max`.
pub async fn linger_then_cleanup(cfg: Config, d: std::sync::Arc<Detach>) {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        if fs::read_to_string(&d.lease).is_ok_and(|t| t != d.token) {
            crate::log(format_args!(
                "detached output {} restarted by another writer; leaving it",
                d.stem
            ));
            return;
        }
        if d.expired(&cfg, SystemTime::now()) {
            let n = d.cleanup();
            crate::log(format_args!(
                "detached output {}: viewer gone, removed {n} files",
                d.stem
            ));
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tcpool-detach-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    const STEM: &str = "0123456789abcdef0123456789abcdef";

    fn cfg_for(root: &Path) -> Config {
        let mut c = Config::minimal();
        c.policy.output_root = root.to_string_lossy().into_owned();
        c
    }

    fn args_for(dir: &Path) -> Vec<String> {
        vec![
            "-i".into(),
            "/media/x.mkv".into(),
            "-hls_segment_filename".into(),
            format!("{}/{STEM}%d.ts", dir.display()),
            format!("{}/{STEM}.m3u8", dir.display()),
        ]
    }

    #[test]
    fn eligibility() {
        let root = tmp("elig");
        let dir = root.join("jf");
        fs::create_dir_all(&dir).unwrap();
        let cfg = cfg_for(&root);
        let ka = format!("{}/jf/.jellymesh-alive/s1", root.display());
        // no keepalive -> attached, like today
        assert!(Detach::from_job(&cfg, &args_for(&dir), "")
            .unwrap()
            .is_none());
        // no lease on disk -> attached
        assert!(Detach::from_job(&cfg, &args_for(&dir), &ka)
            .unwrap()
            .is_none());
        fs::write(dir.join(format!("{STEM}.tcpool.lock")), "tok").unwrap();
        let d = Detach::from_job(&cfg, &args_for(&dir), &ka)
            .unwrap()
            .unwrap();
        assert_eq!(d.token, "tok");
        // keepalive outside the output root is a policy error
        assert!(Detach::from_job(&cfg, &args_for(&dir), "/etc/passwd").is_err());
        assert!(
            Detach::from_job(&cfg, &args_for(&dir), &format!("{}/../x", root.display())).is_err()
        );
        // disabled by config
        let mut off = cfg.clone();
        off.detach = false;
        assert!(Detach::from_job(&off, &args_for(&dir), &ka)
            .unwrap()
            .is_none());
        // a non-hex stem is never eligible (no safe delete prefix)
        let bad = vec![format!("{}/p.m3u8", dir.display())];
        assert!(Detach::from_job(&cfg, &bad, &ka).unwrap().is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn takeover_names_our_token_only() {
        let root = tmp("take");
        let dir = root.join("jf");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{STEM}.tcpool.lock")), "tok-a").unwrap();
        let cfg = cfg_for(&root);
        let ka = format!("{}/jf/.jellymesh-alive/s1", root.display());
        let d = Detach::from_job(&cfg, &args_for(&dir), &ka)
            .unwrap()
            .unwrap();
        assert!(!d.takeover_requested());
        fs::write(dir.join(format!("{STEM}.tcpool.takeover")), "tok-old").unwrap();
        assert!(
            !d.takeover_requested(),
            "a stale request for an older holder is ignored"
        );
        fs::write(dir.join(format!("{STEM}.tcpool.takeover")), "tok-a").unwrap();
        assert!(d.takeover_requested());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn release_and_cleanup_respect_a_new_holder() {
        let root = tmp("clean");
        let dir = root.join("jf");
        fs::create_dir_all(dir.join(".jellymesh-alive")).unwrap();
        let lock = dir.join(format!("{STEM}.tcpool.lock"));
        fs::write(&lock, "tok-a").unwrap();
        for f in ["0.ts", "1.ts", "2.ts.tmp", ".m3u8", ".worker"] {
            fs::write(dir.join(format!("{STEM}{f}")), "x").unwrap();
        }
        fs::write(dir.join("ffffffffffffffffffffffffffffffff0.ts"), "other").unwrap();
        let ka = dir.join(".jellymesh-alive/s1");
        fs::write(&ka, "playing").unwrap();
        let cfg = cfg_for(&root);
        let d = Detach::from_job(&cfg, &args_for(&dir), &ka.to_string_lossy())
            .unwrap()
            .unwrap();
        // someone else now holds the output: neither release nor cleanup touch it
        fs::write(&lock, "tok-b").unwrap();
        d.release();
        assert!(lock.exists());
        assert_eq!(d.cleanup(), 0);
        assert!(dir.join(format!("{STEM}0.ts")).exists());
        // ours again: cleanup removes every <stem>* file (incl. the lock) and the keepalive,
        // and nothing of another output
        fs::write(&lock, "tok-a").unwrap();
        assert_eq!(d.cleanup(), 6);
        assert!(!ka.exists());
        assert!(dir.join("ffffffffffffffffffffffffffffffff0.ts").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn expiry_only_once_detached() {
        let root = tmp("exp");
        let dir = root.join("jf");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{STEM}.tcpool.lock")), "t").unwrap();
        let mut cfg = cfg_for(&root);
        cfg.orphan_idle = Duration::from_secs(1);
        let ka = format!("{}/jf/.jellymesh-alive/s1", root.display());
        let d = Detach::from_job(&cfg, &args_for(&dir), &ka)
            .unwrap()
            .unwrap();
        let later = SystemTime::now() + Duration::from_secs(10);
        assert!(!d.expired(&cfg, later), "an attached job never expires");
        assert!(d.mark_detached());
        assert!(!d.mark_detached());
        assert!(!d.expired(&cfg, SystemTime::now()));
        assert!(d.expired(&cfg, later));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_prefix_refuses_short_stems() {
        let root = tmp("short");
        fs::write(root.join("a.ts"), "x").unwrap();
        assert_eq!(delete_prefix(&root, ""), 0);
        assert_eq!(delete_prefix(&root, "a"), 0);
        assert!(root.join("a.ts").exists());
        let _ = fs::remove_dir_all(&root);
    }
}
