//! `endpoint_probe --base-url URL --model NAME [--out DIR] [--only P1,P2] [--runs N]`
//! The key is read from `PROBE_API_KEY` only, never from arguments.
use agentkit_providers::probe::{Endpoint, Secret, Status, run_checks};

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(base), Some(model)) = (flag(&args, "--base-url"), flag(&args, "--model")) else {
        eprintln!(
            "usage: endpoint_probe --base-url URL --model NAME [--out DIR] [--only P1,P2] [--runs N]\nkey: env PROBE_API_KEY"
        );
        std::process::exit(2);
    };
    let key = Secret::new(std::env::var("PROBE_API_KEY").unwrap_or_default());
    let only: Vec<String> = flag(&args, "--only")
        .map(|v| v.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    let runs = flag(&args, "--runs")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let report = run_checks(&Endpoint::new(&base, &model, key), &only, runs).await;
    let out = std::path::PathBuf::from(flag(&args, "--out").unwrap_or_else(|| ".".into()));
    let written = std::fs::create_dir_all(&out)
        .and_then(|()| std::fs::write(out.join("probe-report.json"), report.to_json()))
        .and_then(|()| std::fs::write(out.join("probe-report.md"), report.to_markdown()));
    if let Err(error) = written {
        eprintln!("cannot write report: {error}");
        std::process::exit(2);
    }
    println!("{}", report.to_markdown());
    let failed = report
        .results
        .iter()
        .any(|r| matches!(r.status, Status::Fail | Status::Error));
    std::process::exit(i32::from(failed));
}
