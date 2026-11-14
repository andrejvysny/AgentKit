use super::capture::{CaptureConfig, MutationSchedule};

pub struct Scenario {
    pub name: &'static str,
    pub config: CaptureConfig,
}

fn config(protocol: &str, profile: &str, schedule: MutationSchedule) -> CaptureConfig {
    CaptureConfig {
        protocol: protocol.into(),
        profile: profile.into(),
        inputs: (0..30).map(|n| format!("turn {n}")).collect(),
        frames: vec![],
        tool_results: (0..30)
            .map(|n| "x".repeat(if n == 0 { 7000 } else { 100 }))
            .collect(),
        mutations: false,
        schedule: Some(schedule),
    }
}

pub fn baseline_scenarios() -> Vec<Scenario> {
    let mut result = vec![];
    for protocol in ["chat-completions", "responses"] {
        for profile in ["chat", "coding"] {
            let control = MutationSchedule::default();
            let combined = MutationSchedule {
                rules: true,
                selection: true,
                tools: true,
            };
            result.push(Scenario {
                name: "control",
                config: config(protocol, profile, control),
            });
            result.push(Scenario {
                name: "combined",
                config: config(protocol, profile, combined),
            });
            if protocol == "responses" {
                for (name, schedule) in [
                    (
                        "selection",
                        MutationSchedule {
                            selection: true,
                            ..Default::default()
                        },
                    ),
                    (
                        "tools",
                        MutationSchedule {
                            tools: true,
                            ..Default::default()
                        },
                    ),
                ] {
                    result.push(Scenario {
                        name,
                        config: config(protocol, profile, schedule),
                    });
                }
            }
        }
    }
    result
}
