// `ENGINE_E2E` set at build time un-ignores the real-ffmpeg tests.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(engine_e2e)");
    println!("cargo::rerun-if-env-changed=ENGINE_E2E");
    if std::env::var_os("ENGINE_E2E").is_some() {
        println!("cargo::rustc-cfg=engine_e2e");
    }
}
