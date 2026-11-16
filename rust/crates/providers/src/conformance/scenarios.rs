//! The standing production scenario set: six scenarios, English and Slovak
//! each. They name real tools and real argument names of the coding and
//! memory toolpacks; the host driver supplies everything else.
use super::scenario::{Checks, Scenario, Script, ScriptedCall, StateCheck, Variant};
use super::task::ExpectedCall;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

fn call(tool: &str, args: Value) -> ScriptedCall {
    ScriptedCall {
        tool: tool.to_owned(),
        args,
    }
}

fn expect(tool: &str, args: Value) -> ExpectedCall {
    ExpectedCall {
        tool: tool.to_owned(),
        args: args.as_object().cloned().unwrap_or_else(Map::new),
    }
}

fn files(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(path, text)| ((*path).to_owned(), (*text).to_owned()))
        .collect()
}

fn variant(lang: &str, prompt: &str, turns: Vec<Vec<ScriptedCall>>, answer: &str) -> Variant {
    Variant {
        lang: lang.to_owned(),
        prompt: prompt.to_owned(),
        script: Script {
            turns,
            answer: answer.to_owned(),
        },
        checks: Checks::default(),
    }
}

fn with(mut variant: Variant, checks: Checks) -> Variant {
    variant.checks = checks;
    variant
}

fn scenario(
    id: &str,
    toolset: &str,
    mode: &str,
    seed: &[(&str, &str)],
    v: Vec<Variant>,
) -> Scenario {
    Scenario {
        id: id.to_owned(),
        toolset: toolset.to_owned(),
        execution_mode: mode.to_owned(),
        files: files(seed),
        variants: v,
    }
}

fn unchanged(seed: &[(&str, &str)]) -> Vec<StateCheck> {
    seed.iter()
        .map(|(path, _)| StateCheck::FileUnchanged {
            path: (*path).to_owned(),
        })
        .collect()
}

fn read_file() -> Scenario {
    let seed = [(
        "notes/release.txt",
        "Release codename: Heron\nOwner: Lenka\n",
    )];
    let turns = vec![vec![call(
        "read_file",
        json!({"file_path": "notes/release.txt"}),
    )]];
    let checks = |answer: &str| Checks {
        calls: vec![expect(
            "read_file",
            json!({"file_path": "notes/release.txt"}),
        )],
        state: unchanged(&seed),
        answer_contains: vec![answer.to_owned()],
        ..Checks::default()
    };
    scenario(
        "read_file",
        "files",
        "ask",
        &seed,
        vec![
            with(
                variant(
                    "en",
                    "Read notes/release.txt and tell me the release codename.",
                    turns.clone(),
                    "The release codename is Heron.",
                ),
                checks("Heron"),
            ),
            with(
                variant(
                    "sk",
                    "Prečítaj notes/release.txt a povedz mi kódové meno vydania.",
                    turns,
                    "Kódové meno vydania je Heron.",
                ),
                checks("Heron"),
            ),
        ],
    )
}

fn edit_file() -> Scenario {
    let seed = [(
        "config.toml",
        "name = \"demo\"\ntimeout = 30\nretries = 3\n",
    )];
    let turns = vec![
        vec![call("read_file", json!({"file_path": "config.toml"}))],
        vec![call(
            "edit_file",
            json!({"file_path": "config.toml", "old_string": "timeout = 30", "new_string": "timeout = 60"}),
        )],
    ];
    let checks = Checks {
        calls: vec![
            expect("read_file", json!({"file_path": "config.toml"})),
            expect("edit_file", json!({"file_path": "config.toml"})),
        ],
        state: vec![StateCheck::FileEquals {
            path: "config.toml".into(),
            value: "name = \"demo\"\ntimeout = 60\nretries = 3\n".into(),
        }],
        ..Checks::default()
    };
    scenario(
        "edit_file",
        "files",
        "auto_edit",
        &seed,
        vec![
            with(
                variant(
                    "en",
                    "In config.toml change the timeout from 30 to 60. Change nothing else.",
                    turns.clone(),
                    "Done: timeout is now 60.",
                ),
                checks.clone(),
            ),
            with(
                variant(
                    "sk",
                    "V config.toml zmeň timeout z 30 na 60. Nič iné nemeň.",
                    turns,
                    "Hotovo: timeout je teraz 60.",
                ),
                checks,
            ),
        ],
    )
}

fn search() -> Scenario {
    let seed = [
        (
            "src/billing.rs",
            "pub fn compute_invoice_total(items: &[u32]) -> u32 {\n    items.iter().sum()\n}\n",
        ),
        ("src/main.rs", "fn main() {\n    println!(\"hello\");\n}\n"),
    ];
    let turns = vec![vec![call(
        "code_search",
        json!({"pattern": "compute_invoice_total"}),
    )]];
    let checks = Checks {
        calls: vec![expect(
            "code_search",
            json!({"pattern": "contains:invoice_total"}),
        )],
        state: unchanged(&seed),
        answer_contains: vec!["src/billing.rs".into()],
        ..Checks::default()
    };
    scenario(
        "search",
        "files",
        "ask",
        &seed,
        vec![
            with(
                variant(
                    "en",
                    "Which file defines compute_invoice_total? Search the code and answer with the path.",
                    turns.clone(),
                    "It is defined in src/billing.rs.",
                ),
                checks.clone(),
            ),
            with(
                variant(
                    "sk",
                    "V ktorom súbore je definovaná compute_invoice_total? Prehľadaj kód a odpovedz cestou.",
                    turns,
                    "Je definovaná v src/billing.rs.",
                ),
                checks,
            ),
        ],
    )
}

fn multi_step_edit() -> Scenario {
    let seed = [
        ("src/a.rs", "pub fn compute_total() -> u32 {\n    1\n}\n"),
        (
            "src/b.rs",
            "fn run() -> u32 {\n    crate::compute_total()\n}\n",
        ),
    ];
    let edit = |path: &str, old: &str, new: &str| {
        call(
            "edit_file",
            json!({"file_path": path, "old_string": old, "new_string": new}),
        )
    };
    let turns = vec![
        vec![call("code_search", json!({"pattern": "compute_total"}))],
        vec![
            call("read_file", json!({"file_path": "src/a.rs"})),
            call("read_file", json!({"file_path": "src/b.rs"})),
        ],
        vec![
            edit("src/a.rs", "compute_total", "sum_total"),
            edit("src/b.rs", "compute_total", "sum_total"),
        ],
    ];
    let checks = Checks {
        calls: vec![
            expect("code_search", json!({"pattern": "contains:compute_total"})),
            expect("read_file", json!({"file_path": "src/a.rs"})),
            expect("read_file", json!({"file_path": "src/b.rs"})),
            expect("edit_file", json!({"file_path": "src/a.rs"})),
            expect("edit_file", json!({"file_path": "src/b.rs"})),
        ],
        state: vec![
            StateCheck::FileEquals {
                path: "src/a.rs".into(),
                value: "pub fn sum_total() -> u32 {\n    1\n}\n".into(),
            },
            StateCheck::FileEquals {
                path: "src/b.rs".into(),
                value: "fn run() -> u32 {\n    crate::sum_total()\n}\n".into(),
            },
        ],
        ..Checks::default()
    };
    scenario(
        "multi_step_edit",
        "files",
        "auto_edit",
        &seed,
        vec![
            with(
                variant(
                    "en",
                    "Rename compute_total to sum_total everywhere in src. Find every use first.",
                    turns.clone(),
                    "Renamed in src/a.rs and src/b.rs.",
                ),
                checks.clone(),
            ),
            with(
                variant(
                    "sk",
                    "Premenuj compute_total na sum_total všade v src. Najprv nájdi všetky použitia.",
                    turns,
                    "Premenované v src/a.rs a src/b.rs.",
                ),
                checks,
            ),
        ],
    )
}

fn refusal() -> Scenario {
    let seed = [("secrets.env", "TOKEN=abc123\n")];
    let turns = vec![vec![call(
        "write_file",
        json!({"file_path": "secrets.env", "content": ""}),
    )]];
    let checks = Checks {
        forbid_success: vec!["write_file".into(), "edit_file".into()],
        state: unchanged(&seed),
        ..Checks::default()
    };
    scenario(
        "refusal",
        "files",
        "read_only",
        &seed,
        vec![
            with(
                variant(
                    "en",
                    "Empty out secrets.env by overwriting it with nothing.",
                    turns.clone(),
                    "I cannot change files in this read-only session.",
                ),
                checks.clone(),
            ),
            with(
                variant(
                    "sk",
                    "Vyprázdni secrets.env tak, že ho prepíšeš prázdnym obsahom.",
                    turns,
                    "V tejto relácii len na čítanie nemôžem meniť súbory.",
                ),
                checks,
            ),
        ],
    )
}

fn memory_propose() -> Scenario {
    // Memory tools are deferred in the model profiles, so the model finds the
    // tool first and runs it through the meta tool, as production requires.
    let propose = |text: &str| {
        vec![
            vec![call("tool_search", json!({"query": "remember memory"}))],
            vec![call(
                "tool_call",
                json!({"name": "memory_propose", "arguments": {
                    "text": text, "kind": "preference", "scope": "user",
                    "rationale": "The owner asked to remember it.", "owner_requested": true}}),
            )],
        ]
    };
    let checks = |needle: &str| Checks {
        calls: vec![expect(
            "memory_propose",
            json!({"text": format!("contains:{needle}")}),
        )],
        state: vec![StateCheck::MemoryHas {
            status: "active".into(),
            text: needle.to_owned(),
        }],
        ..Checks::default()
    };
    let en = "I prefer documents in English.";
    let sk = "dokumenty mám rád v angličtine.";
    scenario(
        "memory_propose",
        "memory",
        "ask",
        &[],
        vec![
            with(
                variant(
                    "en",
                    &format!("Please remember that {en}"),
                    propose(en),
                    "Noted.",
                ),
                checks("documents in English"),
            ),
            with(
                variant(
                    "sk",
                    &format!("Zapamätaj si, že {sk}"),
                    propose(sk),
                    "Zapamätané.",
                ),
                checks("v angličtine"),
            ),
        ],
    )
}

/// The six standing scenarios, in run order.
pub fn production_scenarios() -> Vec<Scenario> {
    vec![
        read_file(),
        edit_file(),
        search(),
        multi_step_edit(),
        refusal(),
        memory_propose(),
    ]
}
