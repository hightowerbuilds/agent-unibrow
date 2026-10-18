//! Lets the app binary find libcef.so (and its subprocesses find it too)
//! without LD_LIBRARY_PATH: the CEF directory goes into the binary's rpath.
fn main() {
    if let Ok(dir) = std::env::var("DEP_UNIBROW_CEF_DIR") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{dir}");
    }
}
