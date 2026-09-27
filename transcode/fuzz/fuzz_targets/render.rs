//! Fuzz target (b): `render()`/`translate()`/`render_trickplay()` must never panic, for any
//! backend, on arbitrary argv (accepted by `validate()` or not -- the shim only calls these
//! after `validate()` passes, but the functions themselves must degrade gracefully on anything).

#![no_main]

use libfuzzer_sys::fuzz_target;
use tcpool_ir::{render, render_trickplay, translate, Backend, TranslateOpts};
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
        // render_trickplay() takes no gpu_filters knob (Shape::Trickplay never moves filters to
        // the GPU -- see its doc comment), but must be just as panic-safe on arbitrary argv.
        let t = render_trickplay(
            &args,
            backend,
            &TranslateOpts {
                pathmap: vec![("/media".to_string(), "/local/media".to_string())],
                gpu_filters: false,
            },
        );
        // Structural invariants that hold for *any* input, trickplay-shaped or not. Stated as
        // deltas (not "at most 1" / "never present"): the input itself may already contain
        // `-stats` or `-hwaccel` tokens for arbitrary fuzzed argv, and render_trickplay() only
        // ever *inserts*, never removes, so those must be counted, not assumed absent.
        let i_in = args.iter().filter(|a| *a == "-i").count();
        let i_out = t.args.iter().filter(|a| *a == "-i").count();
        assert_eq!(
            i_out, i_in,
            "render_trickplay() for {backend:?} changed the number of -i occurrences"
        );
        let stats_in = args.iter().filter(|a| *a == "-stats").count();
        let stats_out = t.args.iter().filter(|a| *a == "-stats").count();
        assert_eq!(
            stats_out,
            stats_in.max(1),
            "render_trickplay() for {backend:?} did not settle -stats at max(existing, 1): {:?}",
            t.args
        );
        let hwaccel_in = args.iter().filter(|a| *a == "-hwaccel").count();
        let hwaccel_out = t.args.iter().filter(|a| *a == "-hwaccel").count();
        let expected_inserted = if backend == Backend::Cpu || i_in == 0 { 0 } else { 1 };
        assert_eq!(
            hwaccel_out,
            hwaccel_in + expected_inserted,
            "render_trickplay() for {backend:?} inserted an unexpected number of -hwaccel args: {:?}",
            t.args
        );
    }
    // Exercise the other IR helpers on the same input: none of these should panic either.
    let _ = tcpool_ir::is_video_copy(&args);
    let _ = tcpool_ir::is_hls_transcode(&args);
    let _ = tcpool_ir::first_segment(&args);
    let _ = tcpool_ir::segment_pattern(&args);
    let _ = tcpool_ir::playlist(&args);
    let _ = tcpool_ir::input_path(&args);
    let _ = tcpool_ir::required_output(&args);
    let _ = tcpool_ir::trickplay_output_dir(&args);
    let _ = tcpool_ir::trickplay_first_frame(&args);
    let _ = Backend::parse("cpu");
});
