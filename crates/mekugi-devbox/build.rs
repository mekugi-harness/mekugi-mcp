fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    // SAFETY: build scripts run single-threaded before invoking tonic-prost-build.
    unsafe {
        std::env::set_var("PROTOC", protoc);
    }
    let proto_file = "../../proto/code_mode_v1.proto";
    println!("cargo:rerun-if-changed={proto_file}");
    tonic_prost_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&[proto_file], &["../../proto"])?;
    Ok(())
}
