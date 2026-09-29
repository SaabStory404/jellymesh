//! Stand-alone DV7 -> 8.1 filter: MPEG-TS on stdin -> rewritten MPEG-TS on stdout, using the
//! agent's own `dv81` + `dv81_ts` modules (compiled in via `#[path]`, byte-for-byte the code the
//! agent runs between ffmpeg#1 and ffmpeg#2). For lab proofs without deploying an agent image:
//!
//! ```text
//! ffmpeg -i src.mkv -map 0:v:0 -c:v copy -copyts -output_ts_offset 10 -muxdelay 0 \
//!     -muxpreload 0 -f mpegts - | dv81_filter [level] | ffmpeg -f mpegts -i - ...
//! ```
//!
//! `level` is the source's DV level (ffprobe `dv_level`, default 6). Stats go to stderr.
#![allow(dead_code)]

#[path = "../src/dv81.rs"]
mod dv81;
#[path = "../src/dv81_ts.rs"]
mod dv81_ts;

use std::io::{Read, Write};

fn transform(data: &[u8]) -> Result<dv81_ts::TransformOut, String> {
    let (out, st) = dv81::convert_access_unit(data)?;
    Ok((out, st.rpus, st.dropped_el))
}

fn main() {
    let level: u8 = std::env::args()
        .nth(1)
        .and_then(|l| l.parse().ok())
        .unwrap_or(6);
    let mut rw = dv81_ts::TsRewriter::new(dv81_ts::DoviDescriptor::profile_81(level), transform);
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let mut buf = vec![0u8; 256 << 10];
    let mut out = Vec::with_capacity(512 << 10);
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("dv81_filter: read: {e}");
                std::process::exit(1);
            }
        };
        out.clear();
        let r = if n == 0 {
            rw.finish(&mut out)
        } else {
            rw.push(&buf[..n], &mut out)
        };
        if let Err(e) = r {
            eprintln!("dv81_filter: {e}");
            std::process::exit(2);
        }
        if stdout.write_all(&out).is_err() {
            break;
        }
        if n == 0 {
            break;
        }
    }
    let _ = stdout.flush();
    let s = rw.stats;
    eprintln!(
        "dv81_filter: {} video frames, {} RPUs rewritten to 8.1, {} EL NALs dropped",
        s.video_pes, s.rpus, s.dropped_el
    );
}
