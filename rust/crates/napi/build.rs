// macOS's linker refuses undefined symbols that the Linux one tolerates.
// The `napi_*` symbols are legitimately absent at LINK time for the shipped
// cdylib: they are provided at LOAD time by the Node/Bun host that dlopens
// the add-on. `cargo test` never needs this flag — the in-crate `#[cfg(test)]`
// stubs satisfy the harness binaries on every platform, and Cargo REJECTS
// `rustc-link-arg-tests` for this package (its test story is the lib target
// plus generated code, with no separate test target to attach the key to).
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-undefined,dynamic_lookup");
    }
    println!("cargo:rustc-check-cfg=cfg()");
}
