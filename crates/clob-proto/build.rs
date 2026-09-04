//! Compiles proto/clob.proto with the real protoc. A system `protoc` is used when the PROTOC
//! environment variable is set; otherwise the binary vendored by `protoc-bin-vendored` is used,
//! so a fresh checkout builds with nothing but cargo.
//!
//! The vendored path is passed to prost through its own configuration rather than by setting
//! `PROTOC` in this process: since edition 2024 `set_var` is unsafe, because another thread may
//! be reading the environment while it is written, and a build script has no way to prove it is
//! alone. Configuring the compiler directly avoids the question.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    let mut prost = prost_build::Config::new();
    if std::env::var_os("PROTOC").is_none() {
        prost.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    }
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        // The descriptor set feeds gRPC reflection, so `grpcurl` works without the .proto file.
        .file_descriptor_set_path(out_dir.join("clob_descriptor.bin"))
        .compile_with_config(prost, &["../../proto/clob.proto"], &["../../proto"])?;
    println!("cargo:rerun-if-changed=../../proto/clob.proto");
    Ok(())
}
