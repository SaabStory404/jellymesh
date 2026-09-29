//! Types for, and the rendering of, the `/status` JSON view of the pool. Served on tcpool-sync's
//! metrics port (TC_METRICS_PORT) next to `/metrics` -- see `metrics::serve`.
//!
//! Read-only and hand-rolled (no new deps), for the Jellyfin "Transcode Pool" plugin (PLAN §11):
//! workers (up/down, last-known caps), units used/capacity per worker, and which codecs are
//! offered with the reason -- `effective = intent AND pool-can`, so the dashboard can say "you
//! turned it off" vs "the P4 cannot do it" instead of just an off checkbox.
//!
//! Three things sync genuinely cannot see yet are reported as null plus a `notes` entry rather
//! than guessed:
//! - **draining**: `Caps` (crates/proto/proto/tcpool.proto) has no draining field, so a worker
//!   that stopped answering Hello is only distinguishable as "down".
//! - **emergency**: the scheduling agent publishes it on its own metrics (:9903); sync has no
//!   client for agent metrics.
//! - **sessions -> worker**: the session id is in the segment path, so this needs a scan of the
//!   shared transcode dir.

use crate::metrics::{survivable, Snapshot};
use serde::Serialize;

/// One Jellyfin encoding offer after reconciliation, in `OFFERS` order.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Offer {
    /// Jellyfin's encoding-config key, e.g. `AllowHevcEncoding`.
    pub option: &'static str,
    /// The output token that key gates, e.g. `hevc`.
    pub token: &'static str,
    /// The user's choice, persisted across cycles (see `main::decide`).
    pub intent: bool,
    /// `intent AND pool-can`: what sync last left in Jellyfin's config, i.e. what Jellyfin offers.
    pub effective: bool,
    /// Decisive reason for `effective`: `enabled`, `user_disabled` or `pool_cannot`.
    pub cause: &'static str,
    /// Human-readable expansion of `cause` for the dashboard row.
    pub reason: String,
    /// Configured workers that cannot output `token` (their names or addresses). Empty when the
    /// pool can. Populated even when the user also disabled the offer, so the page can show both.
    pub lacking: Vec<String>,
}

/// `true` when nothing at all is known about the value: a worker that has never answered Hello.
fn state_of(live: bool, seen_unix: i64) -> &'static str {
    if live {
        "up"
    } else if seen_unix > 0 {
        "down"
    } else {
        "never_seen"
    }
}

/// Render the status document. `now_unix` is passed in (not read here) so the ages are consistent
/// with `synced_unix` and the whole document can be tested against a fixed clock.
pub fn render_json(snap: &Snapshot, now_unix: i64) -> String {
    let workers: Vec<serde_json::Value> = snap
        .workers
        .iter()
        .map(|(name, w)| {
            serde_json::json!({
                "name": name,
                "addr": w.addr,
                "source": w.source,
                "kind": w.kind,
                "state": state_of(w.live, w.seen_unix),
                "outputs": w.outputs,
                "capacity_units": w.capacity,
                "units_used": w.units_used,
                "units_free": (w.capacity - w.units_used).max(0.0),
                "seen_unix": w.seen_unix,
                // -1, not 0: "never seen" must not read as "seen just now" on the dashboard.
                "age_seconds": if w.seen_unix > 0 {
                    (now_unix - w.seen_unix).max(0)
                } else {
                    -1
                },
            })
        })
        .collect();

    let live: Vec<(f64, f64)> = snap
        .workers
        .values()
        .filter(|w| w.live)
        .map(|w| (w.capacity, w.units_used))
        .collect();
    let total_cap: f64 = live.iter().map(|(c, _)| c).sum();
    let total_used: f64 = live.iter().map(|(_, u)| u).sum();

    let doc = serde_json::json!({
        "schema": 1,
        "generated_unix": now_unix,
        "synced_unix": snap.synced_unix,
        "last_success_unix": snap.last_success_unix,
        "workers_configured": snap.workers_configured,
        "workers_live": snap.workers.values().filter(|w| w.live).count(),
        "common_outputs": snap.common,
        "offers": snap.offers,
        "workers": workers,
        "pool": {
            "capacity_units": total_cap,
            "units_used": total_used,
            "survivable": survivable(&live),
        },
        "emergency": serde_json::Value::Null,
        "notes": [
            "draining is not reported: tcpool-agent's Caps has no draining field, so a worker that stops answering Hello reads as down",
            "emergency mode is published on the agent's metrics (:9903); sync does not read agent metrics yet, so it is null here",
            "sessions -> worker is not reported yet: it needs a scan of the shared transcode dir for the per-output lease files",
        ],
    });
    serde_json::to_string_pretty(&doc).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::WorkerInfo;
    use std::collections::BTreeMap;

    fn worker(addr: &str, live: bool, seen: i64) -> WorkerInfo {
        WorkerInfo {
            addr: addr.into(),
            kind: "qsv".into(),
            outputs: vec!["h264".into(), "hevc".into()],
            capacity: 14.0,
            units_used: 2.0,
            live,
            seen_unix: seen,
            source: "dns".into(),
        }
    }

    #[test]
    fn status_reports_worker_state_units_and_offer_reason() {
        let mut workers = BTreeMap::new();
        workers.insert(
            "10.42.0.7:9901".to_string(),
            worker("10.42.0.7:9901", true, 1000),
        );
        workers.insert(
            "10.42.0.9:9901".to_string(),
            worker("10.42.0.9:9901", false, 900),
        );
        workers.insert("p4".to_string(), WorkerInfo::default());
        let snap = Snapshot {
            workers_configured: 3,
            workers,
            common: vec!["h264".into()],
            synced_unix: 1000,
            last_success_unix: 1000,
            offers: vec![Offer {
                option: "AllowHevcEncoding",
                token: "hevc",
                intent: true,
                effective: false,
                cause: "pool_cannot",
                reason: "10.42.0.9:9901 cannot output hevc".into(),
                lacking: vec!["10.42.0.9:9901".into()],
            }],
        };
        let doc = render_json(&snap, 1010);
        let v: serde_json::Value = serde_json::from_str(&doc).expect("valid json");

        assert_eq!(v["schema"], 1);
        assert_eq!(v["workers_live"], 1);
        assert_eq!(v["pool"]["capacity_units"], 14.0);
        assert_eq!(v["pool"]["units_used"], 2.0);
        assert_eq!(v["offers"][0]["cause"], "pool_cannot");
        assert_eq!(v["offers"][0]["effective"], false);
        assert_eq!(v["offers"][0]["intent"], true);
        assert_eq!(v["offers"][0]["lacking"][0], "10.42.0.9:9901");
        assert_eq!(v["emergency"], serde_json::Value::Null);

        let by_name: BTreeMap<&str, &serde_json::Value> = v["workers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| (w["name"].as_str().unwrap(), w))
            .collect();
        assert_eq!(by_name["10.42.0.7:9901"]["state"], "up");
        assert_eq!(by_name["10.42.0.7:9901"]["units_free"], 12.0);
        assert_eq!(by_name["10.42.0.7:9901"]["age_seconds"], 10);
        assert_eq!(by_name["10.42.0.7:9901"]["outputs"][1], "hevc");
        assert_eq!(by_name["10.42.0.9:9901"]["state"], "down");
        // Never seen: no age, not "0 seconds ago", and not counted as live capacity.
        assert_eq!(by_name["p4"]["state"], "never_seen");
        assert_eq!(by_name["p4"]["age_seconds"], -1);
        assert_eq!(v["pool"]["survivable"], false);
    }

    #[test]
    fn status_counts_only_live_workers_in_the_pool_totals() {
        let mut workers = BTreeMap::new();
        workers.insert(
            "arc".to_string(),
            WorkerInfo {
                units_used: 0.0,
                ..worker("10.42.0.7:9901", true, 1000)
            },
        );
        // Down, and carrying units that must NOT be added to the live totals.
        workers.insert("p4".to_string(), worker("10.42.0.9:9901", false, 900));
        let snap = Snapshot {
            workers,
            ..Default::default()
        };
        let v: serde_json::Value = serde_json::from_str(&render_json(&snap, 1000)).unwrap();
        assert_eq!(v["pool"]["capacity_units"], 14.0);
        assert_eq!(v["pool"]["units_used"], 0.0);
        // An idle single live card is survivable; the down worker's 2 units are not live load.
        assert_eq!(v["pool"]["survivable"], true);
    }
}
