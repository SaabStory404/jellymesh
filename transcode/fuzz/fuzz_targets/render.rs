//! Fuzz target (b): `render()`/`translate()` must never panic, for any backend, on arbitrary
//! argv (accepted by `validate()` or not -- the shim only calls these after `validate()` passes,
//! but the functions themselves must degrade gracefully on anything).

#![no_main]

use libfuzzer_sys::fuzz_target;
use tcpool_ir::{render, translate, Backend, TranslateOpts};
use tcpool_ir_fuzz::{fuzz_args, BACKENDS};

fuzz_target!(|data: &[u8]| {
    let args = fuzz_args(data);
    for &backend in BACKENDS.iter() {
        for gpu_filters in [false, true] {
            let opts = TranslateOpts {
                pathmap: vec![("/media".to_string(), "/local/media".to_string())],
                gpu_filters,
            };
            let _ = translate(&args, backend, &opts);
            let _ = render(&args, backend, &opts);
        }
    }
    // Exercise the other IR helpers on the same input: none of these should panic either.
    let _ = tcpool_ir::is_video_copy(&args);
    let _ = tcpool_ir::is_hls_transcode(&args);
    let _ = tcpool_ir::first_segment(&args);
    let _ = tcpool_ir::segment_pattern(&args);
    let _ = tcpool_ir::playlist(&args);
    let _ = tcpool_ir::input_path(&args);
    let _ = tcpool_ir::required_output(&args);
    let _ = Backend::parse("cpu");
});
