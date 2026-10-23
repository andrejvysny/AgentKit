use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[path = "runner_conformance/crash.rs"]
mod crash;
#[path = "runner_conformance/scenarios.rs"]
mod scenarios;
#[path = "runner_conformance/support.rs"]
mod support;

#[derive(Deserialize)]
struct Vectors {
    scenarios: Vec<Scenario>,
}
#[derive(Deserialize)]
struct Scenario {
    name: String,
    expected: Expected,
}
#[derive(Deserialize)]
struct Expected {
    rust: Vec<Value>,
}

#[tokio::test]
async fn runner_fault_conformance() {
    let vectors: Vectors = serde_json::from_str(include_str!(
        "../../../fixtures/conformance/runner-faults-v1.json"
    ))
    .unwrap();
    let directory = support::Directory::new();
    let mut report: BTreeMap<String, BTreeMap<String, Vec<Value>>> = BTreeMap::new();
    for mode in ["memory", "file"] {
        for scenario in &vectors.scenarios {
            if mode == "memory" && scenario.name == "crash_recovery" {
                continue;
            }
            let actual = if scenario.name == "crash_recovery" {
                crash::run(&directory.0).await
            } else {
                let store = if mode == "memory" {
                    agentkit_store::SqliteStore::memory(16).unwrap()
                } else {
                    agentkit_store::SqliteStore::open(
                        directory.0.join(format!("{mode}-{}.db", scenario.name)),
                        16,
                    )
                    .unwrap()
                };
                scenarios::run(store, &scenario.name).await
            };
            assert_eq!(actual, scenario.expected.rust, "{mode}/{}", scenario.name);
            report
                .entry(mode.into())
                .or_default()
                .insert(scenario.name.clone(), actual);
        }
    }
    if let Ok(path) = std::env::var("AGENTKIT_RUNNER_CONFORMANCE_REPORT") {
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    assert_eq!(json!(report["file"].len()), json!(6));
}

#[tokio::test]
async fn runner_crash_child() {
    if let Ok(path) = std::env::var("AGENTKIT_RUNNER_CHILD_DB") {
        crash::child(path).await;
    }
}
