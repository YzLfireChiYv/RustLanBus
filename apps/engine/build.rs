fn main() {
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("build time").as_secs();
    println!("cargo:rustc-env=RLB_ENGINE_BUILD_ID={}+{}", env!("CARGO_PKG_VERSION"), stamp);
    println!("cargo:rerun-if-changed=windows/engine.rc");
    println!("cargo:rerun-if-changed=windows/app.manifest");
    embed_resource::compile_for(
        "windows/engine.rc",
        ["rustlanbus-engine"],
        embed_resource::NONE,
    )
    .manifest_required()
    .expect("the engine administrator manifest must be embedded");
}
