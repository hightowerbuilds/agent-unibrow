//! Exposes the CEF directory chosen by cef-dll-sys (see .cargo/config.toml)
//! to this crate (`UNIBROW_CEF_DIR`) and to dependents (`DEP_UNIBROW_CEF_DIR`).
fn main() {
    let dir = std::env::var("DEP_CEF_DLL_WRAPPER_CEF_DIR").expect("cef-dll-sys didn't report its CEF directory");
    println!("cargo:rustc-env=UNIBROW_CEF_DIR={dir}");
    println!("cargo::metadata=DIR={dir}");
    println!("cargo::rerun-if-env-changed=CEF_PATH");
}
