fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(nvml)");

    // NVML (the NVIDIA management library) only exists on Linux and Windows.
    // Apple dropped NVIDIA driver support, so the `nvidia` feature is a no-op
    // on macOS and the crate is not even linked there.
    let nvidia = std::env::var_os("CARGO_FEATURE_NVIDIA").is_some();
    let macos = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos");
    if nvidia && !macos {
        println!("cargo::rustc-cfg=nvml");
    }
}
