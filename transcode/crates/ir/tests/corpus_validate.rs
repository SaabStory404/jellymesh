//! Every real Jellyfin command in the corpus passes the allowlist (after the lab path layout).

use tcpool_ir::validate::{validate, Policy};
use tcpool_ir::Shape;

fn split(cmd: &str) -> Vec<String> {
    // Jellyfin's logged command lines use double quotes around paths and filter chains.
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

fn check(file: &str, policy: &Policy, shape: Shape) -> usize {
    let path = format!("{}/../../corpus/{file}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(path).expect("corpus");
    let mut n = 0;
    for (i, line) in text.lines().enumerate() {
        let (_, cmd) = line.split_once('\t').expect("tsv");
        let args: Vec<String> = split(cmd).into_iter().skip(1).collect();
        if let Err(e) = validate(&args, policy, shape) {
            panic!("{file} line {} rejected: {e}", i + 1);
        }
        n += 1;
    }
    n
}

#[test]
fn lab_software_corpus_passes() {
    let p = Policy {
        output_root: "/transcodes".into(),
        ..Policy::default()
    };
    assert_eq!(check("lab-sw.tsv", &p, Shape::Hls), 67);
}

#[test]
fn prod_corpus_passes_with_prod_roots() {
    let p = Policy {
        input_roots: vec!["/data/media".into()],
        read_roots: vec![
            "/config/data/data/subtitles".into(),
            "/config/data/data/attachments".into(),
        ],
        output_root: "/scratch/transcodes".into(),
        trickplay_output_root: None,
    };
    assert_eq!(check("prod-qsv.tsv", &p, Shape::Hls), 67);
}

/// Every line in `lab-trickplay.tsv` (synthetic, see `corpus/README.md`) both classifies as
/// `Shape::Trickplay` and passes the allowlist under a trickplay output root matching the corpus
/// paths -- the corpus test the P2 spec asks for, beyond plain `check()`.
#[test]
fn trickplay_corpus_classifies_and_passes() {
    let path = format!(
        "{}/../../corpus/lab-trickplay.tsv",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(path).expect("corpus");
    let p = Policy {
        trickplay_output_root: Some("/transcodes/trickplay".into()),
        ..Policy::default()
    };
    let mut n = 0;
    for (i, line) in text.lines().enumerate() {
        let (_, cmd) = line.split_once('\t').expect("tsv");
        let args: Vec<String> = split(cmd).into_iter().skip(1).collect();
        assert_eq!(
            tcpool_ir::classify(&args),
            Shape::Trickplay,
            "lab-trickplay.tsv line {} did not classify as Trickplay",
            i + 1
        );
        if let Err(e) = validate(&args, &p, Shape::Trickplay) {
            panic!("lab-trickplay.tsv line {} rejected: {e}", i + 1);
        }
        n += 1;
    }
    assert_eq!(n, 5);
}
