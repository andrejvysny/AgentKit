//! Offline runtime assembly capture; scripted transport ports never use a network.
mod context_eval;

use context_eval::capture::{CaptureConfig, run};
use serde_json::json;
use std::{error::Error, io::Read};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mode = std::env::args().nth(1);
    if matches!(mode.as_deref(), Some("--baseline" | "--baseline-captures")) {
        println!(
            "{}",
            serde_json::to_string(
                &context_eval::report::baseline_with_captures(
                    mode.as_deref() == Some("--baseline-captures")
                )
                .await
            )?
        );
        return Ok(());
    }
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let configs: Vec<CaptureConfig> = serde_json::from_str(&input)?;
    let mut captures = Vec::with_capacity(configs.len());
    for config in configs {
        let captured = run(config.clone()).await;
        captures.push(json!({"config":config,"capture":captured}));
    }
    println!("{}", serde_json::to_string(&captures)?);
    Ok(())
}
