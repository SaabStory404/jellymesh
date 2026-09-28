//! Parity with the spike's Python `agent.translate()` over the lab software-mode corpus.
//! Goldens: `corpus/goldens/spike-translate.json`, generated from `transcode/spike/agent.py`.

use serde::Deserialize;
use std::collections::HashMap;
use tcpool_ir::{first_segment, translate, Backend, TranslateOpts};

#[derive(Deserialize)]
struct Row {
    kind: String,
    input: Vec<String>,
    output: Vec<String>,
    gpu_filters: bool,
    first_segment: String,
}

fn goldens() -> HashMap<String, Vec<Row>> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../corpus/goldens/spike-translate.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).expect("goldens")).expect("json")
}

#[test]
fn translation_matches_spike_for_every_backend() {
    let g = goldens();
    let mut checked = 0;
    for (kind, rows) in &g {
        let backend = Backend::parse(kind).expect("backend");
        let opts = TranslateOpts {
            pathmap: vec![],
            gpu_filters: true,
            ..Default::default()
        };
        for (n, r) in rows.iter().enumerate() {
            let t = translate(&r.input, backend, &opts);
            assert_eq!(t.args, r.output, "{kind} row {n} ({}) args differ", r.kind);
            assert_eq!(t.gpu_filters, r.gpu_filters, "{kind} row {n} gpu_filters");
            checked += 1;
        }
    }
    assert!(
        checked >= 3 * 60,
        "expected the full corpus, checked {checked}"
    );
}

#[test]
fn first_segment_matches_spike() {
    for r in &goldens()["qsv"] {
        assert_eq!(first_segment(&r.input).unwrap_or_default(), r.first_segment);
    }
}
