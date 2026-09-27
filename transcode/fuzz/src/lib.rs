//! Shared helpers for the tcpool-ir fuzz targets.

/// Split a command line the way Jellyfin's logged commands are quoted (double quotes around
/// paths and filter chains, plain spaces elsewhere). Mirrors
/// `crates/ir/tests/corpus_validate.rs`'s `split`, so seed corpora can be raw corpus command
/// lines. Never panics: `chars()` iteration and `String` pushes are infallible, and this is fed
/// `String::from_utf8_lossy` output so it is always valid UTF-8.
pub fn split_argv(cmd: &str) -> Vec<String> {
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

/// Fuzz input -> an argv, dropping a leading `ffmpeg`-path token (as the real corpus lines have)
/// when there is one, so the interesting mutations land on options rather than argv[0].
pub fn fuzz_args(data: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(data);
    let mut args = split_argv(&text);
    if !args.is_empty() {
        args.remove(0);
    }
    args
}

pub const BACKENDS: [tcpool_ir::Backend; 3] = [
    tcpool_ir::Backend::Cpu,
    tcpool_ir::Backend::Qsv,
    tcpool_ir::Backend::Nvenc,
];
