//! Fuzz target (a): `validate()` must never panic on arbitrary argv, valid or not.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tcpool_ir::validate::{validate, Policy};
use tcpool_ir_fuzz::fuzz_args;

fuzz_target!(|data: &[u8]| {
    let args = fuzz_args(data);
    let _ = validate(&args, &Policy::default());
});
