fn main() -> std::io::Result<()> {
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
    // SAFETY: build scripts are single-threaded.
    unsafe { std::env::set_var("PROTOC", protoc) };
    let include = protoc_bin_vendored::include_path().expect("vendored include");
    let files = [
        "../proto/offline_wire_formats.proto",
        "../proto/wire_format.proto",
        "../proto/ukey.proto",
        "../proto/securegcm.proto",
        "../proto/securemessage.proto",
        "../proto/device_to_device_messages.proto",
    ];
    for f in &files {
        println!("cargo:rerun-if-changed={f}");
    }
    prost_build::Config::new()
        .include_file("protos.rs")
        .compile_protos(&files, &[std::path::Path::new("../proto"), include.as_path()])
}
