//! CLI front for the standing-approval fingerprint, used by the Bun
//! differential test (`packages/toolpacks-coding/tests/
//! command-fingerprint-rust.test.ts`) to compute the Rust digest for exactly
//! the inputs the TypeScript authority saw. Reads a JSON spec from the file
//! given in argv[1]: `{"root","cwd","command","environment"|null}`; prints
//! the digest, or `null` for the TS `null` result. A fingerprinting refusal
//! (TS throw) prints `error:<message>` and exits 2 — the TS caller maps the
//! same throw to `null`, so both sides agree on "re-ask".

use std::collections::HashMap;

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: fingerprint_cli <spec.json>");
        std::process::exit(64);
    };
    let raw = std::fs::read_to_string(&path).expect("spec file readable");
    let spec: serde_json::Value = serde_json::from_str(&raw).expect("spec is JSON");
    let root = spec["root"].as_str().expect("root");
    let cwd = spec["cwd"].as_str().expect("cwd");
    let command = spec["command"].as_str().expect("command");
    let environment: HashMap<String, String> = match spec.get("environment") {
        None | Some(serde_json::Value::Null) => {
            // Same fallback as TS `workspace.environment ?? process.env`.
            std::env::vars().collect()
        }
        Some(map) => map
            .as_object()
            .expect("environment is an object or null")
            .iter()
            .map(|(name, value)| (name.clone(), value.as_str().unwrap_or_default().to_owned()))
            .collect(),
    };
    match agentkit_toolpacks_coding::command_fingerprint(root, cwd, command, &environment) {
        Ok(Some(digest)) => println!("{digest}"),
        Ok(None) => println!("null"),
        Err(error) => {
            println!("error:{error}");
            std::process::exit(2);
        }
    }
}
