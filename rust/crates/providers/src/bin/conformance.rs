//! `conformance [--tasks DIR] [--out DIR] [--runs N] [--min-success F] [--max-invalid F]`
//! Mock models by default. A live endpoint needs `--live --base-url URL --model NAME`.
#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let tasks = concat!(env!("CARGO_MANIFEST_DIR"), "/conformance/tasks");
    std::process::exit(agentkit_providers::conformance::cli::run_cli(&args, tasks).await);
}
