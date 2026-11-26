//! `agentkit-sandbox-launcher <config.json>`: the sandbox container
//! launcher. The configuration path may also come from
//! `AGENTKIT_LAUNCHER_CONFIG`.
//!
//! Exit status: 2 when the configuration or startup is refused; 1 when the
//! listener fails after it started.

use std::path::PathBuf;

use agentkit_sandbox_launcher::{Config, StartupError, run};

const CONFIG_ENV: &str = "AGENTKIT_LAUNCHER_CONFIG";

fn main() {
    let Some(path) = std::env::args_os()
        .nth(1)
        .or_else(|| std::env::var_os(CONFIG_ENV))
        .map(PathBuf::from)
    else {
        eprintln!("usage: agentkit-sandbox-launcher <config.json> (or {CONFIG_ENV})");
        std::process::exit(2);
    };
    let config = match Config::load(&path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("agentkit-sandbox-launcher: {error}");
            std::process::exit(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("agentkit-sandbox-launcher: cannot start the runtime: {error}");
            std::process::exit(2);
        }
    };
    let code = match runtime.block_on(run(config)) {
        Ok(()) => 0,
        Err(error @ StartupError::Serve(_)) => {
            eprintln!("agentkit-sandbox-launcher: {error}");
            1
        }
        Err(error) => {
            eprintln!("agentkit-sandbox-launcher: {error}");
            2
        }
    };
    std::process::exit(code);
}
