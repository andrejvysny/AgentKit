// macOS's linker refuses undefined symbols that the Linux one tolerates.
// The `napi_*` symbols are legitimately absent at LINK time on both the
// shipped cdylib and the test harness: they are provided at LOAD time by
// the Node/Bun host that dlopens the add-on (and by the in-crate stubs for
// `cargo test`). Without `-undefined dynamic_lookup` the cdylib cannot be
// built on macOS at all — there is no host binary to link against. This is
// the standard napi-rs macOS link posture, emitted through Cargo's
// supported channels: `rustc-link-arg` for the cdylib target,
// `rustc-link-arg-tests` for the harness binaries.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-undefined,dynamic_lookup");
        println!("cargo:rustc-link-arg-tests=-Wl,-undefined,dynamic_lookup");
    }
    println!("cargo:rustc-check-cfg=cfg()");
}
