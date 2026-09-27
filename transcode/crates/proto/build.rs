fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Vendored protoc: no system dependency for local builds or CI.
    std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    println!("cargo:rerun-if-changed=proto/tcpool.proto");
    tonic_prost_build::configure().compile_protos(&["proto/tcpool.proto"], &["proto"])?;
    Ok(())
}
