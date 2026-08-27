fn main() -> std::io::Result<()> {
    let protos = ["authentication", "client", "config", "conversations", "events", "rpc", "settings", "ukey", "util"]
        .map(|n| format!("proto/{n}.proto"));
    let mut cfg = prost_build::Config::new();
    cfg.default_package_filename("gmproto");
    prost_reflect_build::Builder::new()
        .file_descriptor_set_bytes("crate::gm::proto::FILE_DESCRIPTOR_SET")
        .compile_protos_with_config(cfg, &protos, &["proto/"])?;
    for p in &protos { println!("cargo:rerun-if-changed={p}"); }
    println!("cargo:rerun-if-changed=proto/vendor/pblite.proto");
    Ok(())
}
