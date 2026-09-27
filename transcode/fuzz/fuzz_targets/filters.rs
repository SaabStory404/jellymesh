//! Fuzz target (c): `split_filters`/`filter_opts`/`hw_filters` must never panic on an arbitrary
//! filter-chain string (the `-vf` value), valid ffmpeg filter syntax or not.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tcpool_ir::filters::{filter_opts, hw_filters, split_filters};
use tcpool_ir_fuzz::BACKENDS;

fuzz_target!(|data: &[u8]| {
    let vf = String::from_utf8_lossy(data);
    let vf = vf.as_ref();
    let parts = split_filters(vf);
    for p in &parts {
        let _ = filter_opts(p);
    }
    let _ = filter_opts(vf);
    for &backend in BACKENDS.iter() {
        let _ = hw_filters(vf, backend);
    }
});
