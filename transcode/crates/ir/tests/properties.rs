//! Property tests over the real command corpus (`corpus/lab-sw.tsv`).
//!
//! 1. `render_preserves_validate_for_every_backend`: any job `validate()` accepts still passes
//!    `validate()` after `render()`, for every backend -- translation must never introduce a
//!    disallowed path or option.
//! 2. The `*_always_rejected` tests are the security property: an input path outside the roots,
//!    an extra positional output (however it's smuggled in), or a `movie`/`amovie`/`sendcmd`/
//!    `asendcmd`/`zmq`/`azmq` filter anywhere in a `-vf`/`-filter_complex` graph (however it's
//!    labelled, quoted, escaped or cased) must always be rejected.

use proptest::prelude::*;
use std::sync::OnceLock;
use tcpool_ir::validate::{validate, Policy};
use tcpool_ir::{render, Backend, Shape, TranslateOpts};

/// Jellyfin's logged command lines use double quotes around paths and filter chains (mirrors
/// `tests/corpus_validate.rs`'s `split`).
fn split(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    let mut had = false;
    for c in cmd.chars() {
        match c {
            '"' => {
                in_q = !in_q;
                had = true;
            }
            ' ' if !in_q => {
                if had || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    had = false;
                }
            }
            _ => cur.push(c),
        }
    }
    if had || !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn corpus() -> &'static Vec<Vec<String>> {
    static CORPUS: OnceLock<Vec<Vec<String>>> = OnceLock::new();
    CORPUS.get_or_init(|| {
        let path = format!("{}/../../corpus/lab-sw.tsv", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(path).expect("corpus");
        text.lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(_, cmd)| split(cmd).into_iter().skip(1).collect())
            .collect()
    })
}

fn corpus_idx() -> impl Strategy<Value = usize> {
    0..corpus().len()
}

fn corpus_get(idx: usize) -> Vec<String> {
    corpus()[idx].clone()
}

fn policy() -> Policy {
    Policy {
        output_root: "/transcodes".into(),
        ..Policy::default()
    }
}

/// A generator that mutates a real corpus command: swap the input-root prefix, insert a benign
/// option, or append a benign filter to an existing `-vf`. All three keep a well-formed,
/// still-acceptable job (verified by the `prop_assume!` below) so the render/validate property
/// has something nontrivial to check.
fn mutate(args: &[String], kind: u8, word: &str) -> Vec<String> {
    let mut out = args.to_vec();
    if out.is_empty() {
        return out;
    }
    match kind % 3 {
        0 => {
            // swap paths: both /media and /data/media are allowed input roots.
            for a in out.iter_mut() {
                *a = a.replace("/media", "/data/media");
            }
        }
        1 => {
            // insert a harmless option right before the final output.
            let n = out.len();
            out.splice(n - 1..n - 1, ["-loglevel".to_string(), word.to_string()]);
        }
        _ => {
            // add a filter to the existing chain, if there is one.
            if let Some(i) = out.iter().position(|a| a == "-vf") {
                if let Some(v) = out.get_mut(i + 1) {
                    v.push_str(",eq=brightness=0.1");
                }
            }
        }
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Property 1: for any job `validate()` accepts, `render()`'s output still passes
    /// `validate()`, for every backend.
    #[test]
    fn render_preserves_validate_for_every_backend(
        idx in corpus_idx(),
        kind in 0u8..3,
        word in "[a-z]{3,8}",
    ) {
        let mutated = mutate(&corpus_get(idx), kind, &word);
        let p = policy();
        prop_assume!(validate(&mutated, &p, Shape::Hls).is_ok());

        for backend in [Backend::Cpu, Backend::Qsv, Backend::Nvenc] {
            for gpu_filters in [false, true] {
                let opts = TranslateOpts { pathmap: vec![], gpu_filters };
                let out = render(&mutated, backend, &opts);
                prop_assert!(
                    validate(&out.args, &p, Shape::Hls).is_ok(),
                    "backend {:?} gpu_filters={} broke validate: {:?} -> {:?}",
                    backend, gpu_filters, mutated, out.args
                );
            }
        }
    }
}

fn forbidden_filter_name() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("movie"),
        Just("amovie"),
        Just("sendcmd"),
        Just("asendcmd"),
        Just("zmq"),
        Just("azmq"),
        Just("lv2"),
        Just("ladspa"),
        // case variants: filter-name comparisons must not be case-sensitive
        Just("Movie"),
        Just("MOVIE"),
        Just("SendCmd"),
        Just("ZMQ"),
        Just("AMOVIE"),
    ]
    .prop_map(|s| s.to_string())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// Security property: a forbidden filter anywhere in `-vf`/`-filter_complex`, however it's
    /// labelled, quoted, whitespace-padded, comma-escaped or cased, is always rejected.
    #[test]
    fn forbidden_filters_always_rejected(
        idx in corpus_idx(),
        name in forbidden_filter_name(),
        wrap_label in any::<bool>(),
        quote_value in any::<bool>(),
        lead_ws in " {0,3}",
        trail_ws in " {0,3}",
        via_filter_complex in any::<bool>(),
        prefix_escaped_comma in any::<bool>(),
    ) {
        let value = if quote_value { "'/etc/passwd'".to_string() } else { "/etc/passwd".to_string() };
        let mut chunk = format!("{lead_ws}{name}{trail_ws}={value}");
        if wrap_label {
            chunk = format!("[0:v]{chunk}[x]");
        }
        let graph = if prefix_escaped_comma {
            format!(r"scale=1\,2,{chunk}")
        } else {
            chunk
        };
        let flag = if via_filter_complex { "-filter_complex" } else { "-vf" };

        let mut args = corpus_get(idx);
        prop_assume!(!args.is_empty());
        let n = args.len();
        args.splice(n - 1..n - 1, [flag.to_string(), graph.clone()]);

        prop_assert!(
            validate(&args, &policy(), Shape::Hls).is_err(),
            "accepted a dangerous filter chain {:?} via {}",
            graph, flag
        );
    }
}

fn bad_input_path() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("/etc/passwd"),
        Just("http://evil/x"),
        Just("concat:/media/a|/etc/passwd"),
        Just("file:/media/../etc/passwd"),
        Just("//etc/passwd"),
        Just("relative/media/x"),
        Just("/medias/evil"),
        Just("/media/foo/../../etc/passwd"),
        Just("file:relative/x"),
    ]
    .prop_map(|s| s.to_string())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Security property: an input path outside the configured roots is always rejected.
    #[test]
    fn input_paths_outside_roots_always_rejected(idx in corpus_idx(), bad in bad_input_path()) {
        let mut args = corpus_get(idx);
        let i = args.iter().position(|a| a == "-i");
        prop_assume!(i.is_some());
        let i = i.unwrap();
        args[i + 1] = bad.clone();
        prop_assert!(
            validate(&args, &policy(), Shape::Hls).is_err(),
            "accepted input path outside the roots: {bad}"
        );
    }
}

/// (extra token to splice before the final positional, an optional flag preceding it -- the
/// flag variants specifically probe the "unknown option swallows the next token as its value"
/// class of bypass).
fn bad_extra_output() -> impl Strategy<Value = (String, Option<String>)> {
    prop_oneof![
        Just(("/tmp/leak.mkv".to_string(), None)),
        Just(("/tmp/leak.m3u8".to_string(), None)),
        Just(("/config/leak.mkv".to_string(), None)),
        Just(("/tmp/leak.mkv".to_string(), Some("-xerror".to_string()))),
        Just(("/tmp/leak.mkv".to_string(), Some("-benchmark".to_string()))),
        Just((
            "/tmp/leak.mkv".to_string(),
            Some("-somemadeupopt".to_string())
        )),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Security property: an extra positional output is always rejected, whether it's a bare
    /// second positional or hidden behind an option we don't specially recognise.
    #[test]
    fn extra_output_always_rejected(idx in corpus_idx(), extra in bad_extra_output()) {
        let (path, flag) = extra;
        let mut args = corpus_get(idx);
        prop_assume!(!args.is_empty());
        let n = args.len();
        let mut ins = Vec::new();
        if let Some(f) = flag {
            ins.push(f);
        }
        ins.push(path.clone());
        args.splice(n - 1..n - 1, ins);
        prop_assert!(
            validate(&args, &policy(), Shape::Hls).is_err(),
            "accepted an extra output at {path}"
        );
    }
}
