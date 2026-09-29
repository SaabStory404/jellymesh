//! Fuzz target (d): the trickplay path end-to-end -- `classify()`, `validate()` for
//! `Shape::Trickplay`, and `render_trickplay()` -- on a well-formed trickplay skeleton whose
//! input path and output directory are fuzzed. Structured (not raw argv, unlike the `validate`
//! target) so path-traversal and protocol-smuggling attempts land exactly on the two arguments
//! that matter, instead of being diluted across an argv shape libfuzzer would otherwise have to
//! rediscover from scratch.
//!
//! Mirrors the shim's actual contract: classify, then validate *with the shape classify() just
//! gave* -- not a hardcoded `Shape::Trickplay`. `classify()`'s own doc comment says `Hls` wins
//! when an argv matches both shapes (e.g. a source file whose *name* happens to end in `.m3u8`,
//! which trips `is_hls_transcode`'s "any arg ends with .m3u8" check on the `-i` value, not just
//! the output); that is documented, deliberate precedence, not a bug, so this target skips a
//! sample entirely rather than asserting the classify()/Shape::Trickplay agreement that
//! `is_hls_transcode` deliberately does not always give.
//!
//! Invariants (found as crashes by `cargo fuzz`, not merely asserted) on any argv `validate()`
//! accepts for the `Shape::Trickplay` classify() itself gave it:
//! - the output is under the configured trickplay root, with no `..` traversal and no scheme
//!   (`file:`/`concat:`/`pipe:`/...) smuggled past the `%08d.jpg` pattern check;
//! - the input is under a configured input root, same traversal/protocol checks;
//! - `render_trickplay()`, for every backend, never turns the accepted argv into one that
//!   `validate()` then rejects -- i.e. it never introduces a flag or path outside the allowlist.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tcpool_ir::validate::{validate, Policy};
use tcpool_ir::{classify, render_trickplay, Shape, TranslateOpts};
use tcpool_ir_fuzz::BACKENDS;

const TRICKPLAY_ROOT: &str = "/transcodes/trickplay";
const INPUT_ROOTS: &[&str] = &["/media", "/data/media"];

/// Duplicated from the `validate` fuzz target rather than shared, so the two targets' oracles
/// stay independent.
const DANGEROUS_PROTOCOLS: &[&str] = &[
    "http:", "https:", "concat:", "pipe:", "ftp:", "rtmp:", "tcp:", "udp:", "subfile:", "crypto:",
    "file:",
];

fn has_traversal(v: &str) -> bool {
    let v = v.trim_matches('"');
    v.split('/').any(|c| c == "..")
}

fn has_dangerous_scheme(v: &str) -> bool {
    let v = v.trim_matches('"');
    DANGEROUS_PROTOCOLS.iter().any(|p| v.starts_with(p))
}

fn under_any(v: &str, roots: &[&str]) -> bool {
    let v = v.trim_matches('"');
    roots
        .iter()
        .any(|r| v == *r || v.starts_with(&format!("{r}/")))
}

fn policy() -> Policy {
    Policy {
        trickplay_output_root: Some(TRICKPLAY_ROOT.to_string()),
        ..Policy::default()
    }
}

/// A trickplay-shaped argv around a fuzzed input path and a fuzzed output directory. The frame
/// pattern itself (`%08d.jpg`) is fixed -- fuzzing *that* shape is the `validate`/`filters`
/// targets' job (via `is_trickplay_pattern`, reachable only through `classify`/`validate`, not
/// exported to the fuzz crate); this target's job is the path payload either side of it.
fn skeleton(input: &str, out_dir: &str) -> Vec<String> {
    vec![
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        format!("file:{input}"),
        "-map".into(),
        "0:0".into(),
        "-an".into(),
        "-sn".into(),
        "-vf".into(),
        "setpts=N/23.976/TB,fps=0.1,scale=320:-2".into(),
        "-c:v".into(),
        "mjpeg".into(),
        "-qscale:v".into(),
        "4".into(),
        "-fps_mode".into(),
        "passthrough".into(),
        "-f".into(),
        "image2".into(),
        format!("{out_dir}/%08d.jpg"),
    ]
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    // Split on the first `\n` so libfuzzer can mutate the input path and the output directory
    // independently -- a fixed `len()/2` split point means any length-changing mutation shifts
    // the boundary and corrupts both halves at once, which starves the mutator. No separator ->
    // the whole payload is the output directory against a fixed, already-valid input path.
    let (input, out_dir) = match data.iter().position(|&b| b == b'\n') {
        Some(i) => (&data[..i], &data[i + 1..]),
        None => (b"/media/movies/x.mkv".as_slice(), data),
    };
    // Strip NUL: it can't occur in a real argv token (a C string terminator), and would just
    // make the harness's own naive checks below diverge from validate()'s.
    let input = String::from_utf8_lossy(input).replace('\0', "");
    let out_dir = String::from_utf8_lossy(out_dir).replace('\0', "");

    let args = skeleton(&input, &out_dir);
    let shape = classify(&args); // must not panic
    let p = policy();
    // Mirror the shim: it only ever calls `validate(.., Shape::Trickplay)` for an argv
    // `classify()` itself called Trickplay (see the module doc comment on why classify()'s answer
    // can legitimately be `Hls` here instead).
    if shape != Shape::Trickplay {
        return;
    }
    let result = validate(&args, &p, shape); // must not panic

    if result.is_err() {
        return;
    }

    let out = args.last().unwrap();
    assert!(
        !has_traversal(out),
        "validate() accepted an output with a `..` segment: {out}"
    );
    assert!(
        !has_dangerous_scheme(out),
        "validate() accepted an output with a smuggled scheme: {out}"
    );
    assert!(
        under_any(out, &[TRICKPLAY_ROOT]),
        "validate() accepted an output escaping the trickplay root: {out}"
    );

    assert!(
        !has_traversal(&input),
        "validate() accepted an input with a `..` segment: {input}"
    );
    assert!(
        !has_dangerous_scheme(&input),
        "validate() accepted an input with a smuggled scheme: {input}"
    );
    assert!(
        under_any(&input, INPUT_ROOTS),
        "validate() accepted an input escaping the input roots: {input}"
    );

    // render_trickplay() must never turn an accepted argv into one validate() then rejects, for
    // any backend -- it must never emit a flag or path outside the allowlist.
    for &backend in BACKENDS.iter() {
        let t = render_trickplay(&args, backend, &TranslateOpts::default());
        assert!(
            validate(&t.args, &p, Shape::Trickplay).is_ok(),
            "render_trickplay() for {backend:?} broke validate(): {:?} -> {:?}",
            args,
            t.args
        );
    }
});
