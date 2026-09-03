//! Compiles proto/clob.proto with the real protoc. A system `protoc` is used when the PROTOC
//! environment variable is set; otherwise the binary vendored by `protoc-bin-vendored` is used,
//! so a fresh checkout builds with nothing but cargo.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("PROTOC").is_none() {
        std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    }
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["../../proto/clob.proto"], &["../../proto"])?;
    println!("cargo:rerun-if-changed=../../proto/clob.proto");
    Ok(())
}
