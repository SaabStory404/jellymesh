//! Golden for the production `render()` with P5's calibrated rate control (the default).
//!
//! `corpus/goldens/render-calibrated.json` holds, for every row of `spike-translate.json` (same
//! backend key, same order), the full argv `render()` produces with `RateControl::Calibrated`.
//! `parity.rs` keeps pinning `translate()` to the spike and `render_unaffected.rs` pins the
//! `Legacy` render; this file pins the deliberate difference between them, so any change to the
//! rate-control pass shows up as a reviewable golden diff.
//!
//! Regenerate after an intentional change: `UPDATE_GOLDENS=1 cargo test -p tcpool-ir --test
//! render_calibrated`, then review the JSON diff.

use serde::Deserialize;
use std::collections::BTreeMap;
use tcpool_ir::{render, Backend, RateControl, SourceVideo, TranslateOpts};

#[derive(Deserialize)]
struct Row {
    input: Vec<String>,
}

const SPIKE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../corpus/goldens/spike-translate.json"
);
const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../corpus/goldens/render-calibrated.json"
);

fn rendered() -> BTreeMap<String, Vec<Vec<String>>> {
    let spike: BTreeMap<String, Vec<Row>> =
        serde_json::from_str(&std::fs::read_to_string(SPIKE).expect("spike goldens"))
            .expect("json");
    let opts = TranslateOpts {
        pathmap: vec![],
        gpu_filters: true,
        rate_control: RateControl::Calibrated,
        // P5.1 ladder: every row as if the admission probe saw a 4K 23.976 fps source, so the
        // corpus's 960..3840 scale bounds land on the 720/1080/1440/2160 rungs.
        source: Some(SourceVideo {
            width: 3840,
            height: 2160,
            fps: 24000.0 / 1001.0,
        }),
    };
    spike
        .into_iter()
        .map(|(kind, rows)| {
            let b = Backend::parse(&kind).expect("backend");
            let out = rows
                .iter()
                .map(|r| render(&r.input, b, &opts).args)
                .collect();
            (kind, out)
        })
        .collect()
}

#[test]
fn calibrated_render_matches_golden() {
    let got = rendered();
    if std::env::var_os("UPDATE_GOLDENS").is_some() {
        let mut s = serde_json::to_string_pretty(&got).expect("json");
        s.push('\n');
        std::fs::write(GOLDEN, s).expect("write golden");
        return;
    }
    let want: BTreeMap<String, Vec<Vec<String>>> =
        serde_json::from_str(&std::fs::read_to_string(GOLDEN).expect("golden")).expect("json");
    assert_eq!(got.len(), want.len());
    let mut checked = 0;
    for (kind, rows) in &got {
        assert_eq!(rows.len(), want[kind].len(), "{kind}: row count");
        for (n, (g, w)) in rows.iter().zip(&want[kind]).enumerate() {
            assert_eq!(
                g, w,
                "{kind} row {n}: calibrated render() diverged from the golden"
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 201,
        "expected the full corpus, checked {checked}"
    );
}
