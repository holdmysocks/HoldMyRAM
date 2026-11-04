fn main() {
    // The updater's WinHTTP is delay-loaded: winhttp.dll is mapped only when a check or a
    // download actually runs, so the idle tray process does not carry it.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/DELAYLOAD:winhttp.dll");
        println!("cargo:rustc-link-lib=delayimp");
    }
    tauri_build::build()
}
