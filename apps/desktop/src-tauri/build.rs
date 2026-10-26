fn main() {
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=../src");
    println!("cargo:rerun-if-changed=../dist");
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("build time").as_secs();
    println!("cargo:rustc-env=RLB_DESKTOP_BUILD_ID={}+{}", env!("CARGO_PKG_VERSION"), stamp);
    let windows =
        tauri_build::WindowsAttributes::new().app_manifest(include_str!("windows/app.manifest"));
    tauri_build::try_build(tauri_build::Attributes::new().windows_attributes(windows))
        .expect("failed to run the Tauri build script");
}
