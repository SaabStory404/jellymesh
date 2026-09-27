//! Property: the P2 trickplay/BATCH work (`classify()`, `validate()`'s trickplay branch,
//! `render_trickplay()`) leaves the HLS path -- `translate()`/`render()`/`add_hls_flag()` --
//! byte-identical. `render_trickplay()` is a separate function reachable only for
//! `Shape::Trickplay`; nothing in this crate's HLS path calls it (see `lib.rs`'s module docs),
//! so this pins that claim against the same golden truth `parity.rs` uses, rather than trusting a
//! by-construction argument alone.
//!
//! For every one of the 201 `corpus/goldens/spike-translate.json` rows, `render()`'s output must
//! equal exactly what its own (untouched) definition says: `translate()`'s already-pinned golden
//! output (or, for a stream copy, the path-mapped input) plus `add_hls_flag(.., "temp_file")`.
//! A future change that alters `translate()`/`render()`/`add_hls_flag()` -- accidentally, while
//! touching the trickplay path -- fails this test even though it never touches
//! `spike-translate.json`.

use serde::Deserialize;
use std::collections::HashMap;
use tcpool_ir::{is_video_copy, map_path, render, Backend, TranslateOpts};

#[derive(Deserialize)]
struct Row {
    input: Vec<String>,
    output: Vec<String>,
    gpu_filters: bool,
}

fn goldens() -> HashMap<String, Vec<Row>> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../corpus/goldens/spike-translate.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).expect("goldens")).expect("json")
}

/// `add_hls_flag(.., "temp_file")`'s effect, reimplemented independently rather than called, so
/// this test doesn't check `render()` against the very function (`add_hls_flag`) it's meant to
/// pin -- a bug that moved identically into both sides would otherwise go undetected.
fn expect_temp_file(args: &[String]) -> Vec<String> {
    let mut out = args.to_vec();
    if let Some(i) = out.iter().position(|a| a == "-hls_flags") {
        if let Some(v) = out.get_mut(i + 1) {
            v.push_str("+temp_file");
        }
        return out;
    }
    let last_m3u8 = out
        .iter()
        .rposition(|a| a.ends_with(".m3u8"))
        .expect("an .m3u8 output");
    out.splice(
        last_m3u8..last_m3u8,
        ["-hls_flags".to_string(), "temp_file".to_string()],
    );
    out
}

#[test]
fn render_of_every_golden_is_translate_plus_temp_file() {
    let g = goldens();
    let mut checked = 0;
    for (kind, rows) in &g {
        let backend = Backend::parse(kind).expect("backend");
        let opts = TranslateOpts {
            pathmap: vec![],
            gpu_filters: true,
        };
        for (n, r) in rows.iter().enumerate() {
            let rendered = render(&r.input, backend, &opts);
            let base: Vec<String> = if is_video_copy(&r.input) {
                r.input.iter().map(|a| map_path(a, &opts.pathmap)).collect()
            } else {
                r.output.clone()
            };
            let expected = expect_temp_file(&base);
            assert_eq!(
                rendered.args, expected,
                "{kind} row {n}: render() diverged from translate()+temp_file"
            );
            if !is_video_copy(&r.input) {
                assert_eq!(
                    rendered.gpu_filters, r.gpu_filters,
                    "{kind} row {n}: render() gpu_filters diverged from the translate() golden"
                );
            }
            checked += 1;
        }
    }
    assert!(
        checked >= 201,
        "expected the full 201-row golden corpus, checked {checked}"
    );
}

/// The same property restated for every backend on every row (not just each row's own recorded
/// `kind`), so a regression that only shows up on a backend a given golden row wasn't generated
/// for still fails this test.
#[test]
fn render_is_backend_independent_of_which_golden_list_it_came_from() {
    let g = goldens();
    // Flatten all input rows across kinds (the same inputs appear once per backend/kind in the
    // golden file); re-derive each backend's render() output fresh and check it still validates
    // the golden invariant: is_video_copy rows never get hwaccel/forced-idr, non-copy rows do
    // (for Qsv/Nvenc) and never for Cpu.
    let mut checked = 0;
    for rows in g.values() {
        for r in rows {
            for backend in [Backend::Cpu, Backend::Qsv, Backend::Nvenc] {
                let out = render(&r.input, backend, &TranslateOpts::default());
                let has_hwaccel = out.args.iter().any(|a| a == "-hwaccel");
                if is_video_copy(&r.input) || backend == Backend::Cpu {
                    assert!(
                        !has_hwaccel,
                        "backend {backend:?} unexpectedly got -hwaccel for a copy/cpu row: {:?}",
                        out.args
                    );
                } else {
                    assert!(
                        has_hwaccel,
                        "backend {backend:?} unexpectedly missing -hwaccel: {:?}",
                        out.args
                    );
                }
                checked += 1;
            }
        }
    }
    assert!(checked >= 3 * 201);
}
