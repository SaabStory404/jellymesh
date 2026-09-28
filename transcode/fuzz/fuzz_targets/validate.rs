//! Fuzz target (a): `classify()` and `validate()` must never panic on arbitrary argv, valid or
//! not, for any `Shape` -- including a mismatched one (the shim always passes `classify()`'s own
//! answer, but `validate()` itself must degrade gracefully on any combination).
//!
//! Also checks, independently of `validate.rs`'s own allowlist constants (duplicated here on
//! purpose so a bug in that module's lists can't hide the same bug from this check): whatever
//! argv `validate()` accepts for `Shape::Hls` or `Shape::Trickplay` never contains a
//! script/attachment/report option, and its final (output) positional is under the matching
//! configured root with no `..` path-traversal segment and no smuggled protocol scheme.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tcpool_ir::validate::{validate, Policy};
use tcpool_ir::{classify, Shape};
use tcpool_ir_fuzz::fuzz_args;

const HLS_OUTPUT_ROOT: &str = "/transcodes";
const TRICKPLAY_ROOT: &str = "/transcodes/trickplay";

/// A second, independently-written "must never appear on accepted argv" list -- not `use`d from
/// `crates/ir/src/validate.rs`'s `FORBIDDEN_OPTS`, so a bug there (an entry dropped, a typo) is
/// still caught here.
const NEVER_ON_ACCEPTED_ARGV: &[&str] = &[
    "-filter_script",
    "-filter_complex_script",
    "-/filter",
    "-/filter_complex",
    "-attach",
    "-dump_attachment",
    "-report",
    "-protocol_whitelist",
    "-protocol_blacklist",
    "-lavfi",
    "-sdp_file",
    "-vstats_file",
    "-passlogfile",
    "-progress",
];

/// Protocol prefixes ffmpeg understands that would let an accepted job read/write somewhere the
/// roots don't cover.
const DANGEROUS_PROTOCOLS: &[&str] = &[
    "http:", "https:", "concat:", "pipe:", "ftp:", "rtmp:", "tcp:", "udp:", "subfile:", "crypto:",
];

fn has_traversal_or_protocol(raw: &str) -> bool {
    let bare = raw.trim_matches('"');
    if bare.split('/').any(|c| c == "..") {
        return true;
    }
    DANGEROUS_PROTOCOLS.iter().any(|p| bare.starts_with(p))
}

fn under_root(raw: &str, root: &str) -> bool {
    let bare = raw.trim_matches('"');
    let root = root.trim_end_matches('/');
    bare == root || bare.starts_with(&format!("{root}/"))
}

fn policy() -> Policy {
    Policy {
        output_root: HLS_OUTPUT_ROOT.to_string(),
        trickplay_output_root: Some(TRICKPLAY_ROOT.to_string()),
        ..Policy::default()
    }
}

fuzz_target!(|data: &[u8]| {
    let args = fuzz_args(data);

    // classify() must never panic, on anything.
    let _ = classify(&args);

    let p = policy();
    for shape in [Shape::Hls, Shape::Trickplay, Shape::Other] {
        let result = validate(&args, &p, shape); // must never panic

        let root = match shape {
            Shape::Hls => HLS_OUTPUT_ROOT,
            Shape::Trickplay => TRICKPLAY_ROOT,
            Shape::Other => continue, // validate() always rejects Shape::Other; nothing to check
        };

        if result.is_ok() {
            for a in &args {
                assert!(
                    !NEVER_ON_ACCEPTED_ARGV.contains(&a.as_str()),
                    "validate() accepted a forbidden option {a} for {shape:?}: {args:?}"
                );
            }
            let out = args
                .last()
                .expect("validate() requires at least one positional output");
            assert!(
                !has_traversal_or_protocol(out),
                "validate() accepted a traversal/protocol-smuggled output for {shape:?}: {out}"
            );
            assert!(
                under_root(out, root),
                "validate() accepted an output outside its {shape:?} root: {out}"
            );
        }
    }
});
