fn main() {
    if std::env::var_os("CARGO_FEATURE_RUNTIME").is_some()
        && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux")
    {
        // CEF interposes libc functions with RTLD_NEXT; it must precede libc.
        println!("cargo:rustc-link-lib=cef");
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
    }
}
