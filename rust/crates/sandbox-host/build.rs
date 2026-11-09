//! Emits `worker_cli.rs`: the worker crate's CLI entry-point source with
//! inner doc comments (`//!`) rewritten to ordinary comments, the only
//! transformation that makes it legal inside an `include!` expansion. Code
//! is byte-identical otherwise; rerun-on-change keeps the testbin faithful.

use std::io::Write as _;

fn main() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../sandbox-worker/src/bin/agentkit-sandbox-worker.rs");
    println!("cargo:rerun-if-changed={}", src.display());
    println!("cargo:rerun-if-changed=build.rs");
    let text =
        std::fs::read_to_string(&src).unwrap_or_else(|e| panic!("read {}: {e}", src.display()));
    let out =
        std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("worker_cli.rs");
    let mut f = std::fs::File::create(&out).expect("create worker_cli.rs");
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("//!") {
            writeln!(f, "//{rest}").expect("write");
        } else {
            writeln!(f, "{line}").expect("write");
        }
    }
    f.flush().expect("flush");
}
