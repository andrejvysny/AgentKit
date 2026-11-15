//! Command-line driver shared by the AgentKit binary and host bindings (OneMind).
use super::{
    LiveConfig, MockMode, MockModel, Model, Thresholds, live_model, load_dir, run_matrix,
};
use crate::probe::Secret;
use std::path::PathBuf;

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn models(args: &[String]) -> Result<Vec<Box<dyn Model>>, String> {
    let live = args.iter().any(|a| a == "--live");
    match (live, flag(args, "--base-url"), flag(args, "--model")) {
        (true, Some(base_url), Some(model)) => {
            let key = Secret::new(std::env::var("CONFORMANCE_API_KEY").unwrap_or_default());
            let config = LiveConfig { base_url, model, key };
            Ok(vec![Box::new(live_model(config, live)?) as Box<dyn Model>])
        }
        (true, _, _) => Err("--live needs --base-url and --model".to_owned()),
        (false, None, None) => Ok(vec![
            Box::new(MockModel::new("mock-perfect", MockMode::Perfect)) as Box<dyn Model>,
            Box::new(MockModel::new("mock-flaky", MockMode::FailEvery(5))),
        ]),
        (false, _, _) => Err("an endpoint needs the explicit --live flag; nothing was run".into()),
    }
}

/// Runs the matrix and writes `conformance-matrix.{json,md}`; returns the exit code.
/// Flags: `--tasks DIR --out DIR --runs N --min-success F --max-invalid F`
/// and, for a live endpoint only, `--live --base-url URL --model NAME`
/// (key from `CONFORMANCE_API_KEY`, never from arguments).
pub async fn run_cli(args: &[String], default_tasks: &str) -> i32 {
    match run(args, default_tasks).await {
        Ok(markdown) => {
            println!("{markdown}");
            0
        }
        Err(message) => {
            eprintln!("{message}");
            2
        }
    }
}

async fn run(args: &[String], default_tasks: &str) -> Result<String, String> {
    let models = models(args)?;
    let dir = PathBuf::from(flag(args, "--tasks").unwrap_or_else(|| default_tasks.to_owned()));
    let tasks = load_dir(&dir)?;
    let runs = flag(args, "--runs").and_then(|v| v.parse().ok()).unwrap_or(3);
    let defaults = Thresholds::default();
    let thresholds = Thresholds {
        min_success: flag(args, "--min-success")
            .and_then(|v| v.parse().ok())
            .unwrap_or(defaults.min_success),
        max_invalid: flag(args, "--max-invalid")
            .and_then(|v| v.parse().ok())
            .unwrap_or(defaults.max_invalid),
    };
    let refs: Vec<&dyn Model> = models.iter().map(|m| m.as_ref()).collect();
    let matrix = run_matrix(&refs, &tasks, runs, thresholds).await;
    let out = PathBuf::from(flag(args, "--out").unwrap_or_else(|| ".".into()));
    std::fs::create_dir_all(&out)
        .and_then(|()| std::fs::write(out.join("conformance-matrix.json"), matrix.to_json()))
        .and_then(|()| std::fs::write(out.join("conformance-matrix.md"), matrix.to_markdown()))
        .map_err(|e| format!("cannot write report: {e}"))?;
    Ok(matrix.to_markdown())
}
