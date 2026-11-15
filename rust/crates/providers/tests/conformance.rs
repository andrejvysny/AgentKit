//! Conformance runner: task parsing, the qualification formula and the live gate.
use agentkit_contracts::AiToolOutcomeClass as Class;
use agentkit_providers::conformance::{
    CallRecord, LiveConfig, Matrix, MockMode, MockModel, Model, ModelCall, ModelTurn, RunCtx,
    RunOutcome, Task, Thresholds, build_matrix, live_model, load_dir, parse_task_file, run_matrix,
    run_task,
};
use agentkit_providers::probe::Secret;
use async_trait::async_trait;
use serde_json::Value;
use std::path::Path;

fn seed_tasks() -> Vec<Task> {
    load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance/tasks")).expect("seed tasks")
}

fn only(tasks: &[Task], toolset: &str) -> Vec<Task> {
    tasks.iter().filter(|t| t.toolset == toolset).cloned().collect()
}

#[test]
fn seed_files_parse_with_three_tasks_and_a_slovak_one_per_toolset() {
    let tasks = seed_tasks();
    for toolset in ["files", "shell", "git", "plan", "code_intel"] {
        let set = only(&tasks, toolset);
        assert_eq!(set.len(), 3, "{toolset}");
        assert!(set.iter().any(|t| t.lang == "sk"), "{toolset} has no Slovak task");
        assert!(set.iter().all(|t| t.max_turns >= 1 && !t.expect.calls.is_empty()));
    }
}

#[test]
fn malformed_task_files_are_rejected() {
    let ok = r#"{"toolset":"t","tasks":[{"id":"a","prompt":"p",
        "allowed_tools":[{"name":"x","effect":{"kind":"reply","text":"r"}}],
        "expect":{"calls":[{"tool":"x"}]}}]}"#;
    let tasks = parse_task_file(ok).expect("valid file");
    assert_eq!(tasks[0].toolset, "t");
    assert_eq!(tasks[0].max_turns, 6);
    assert_eq!(tasks[0].lang, "en");

    let unknown_field = ok.replace(r#""id":"a""#, r#""id":"a","bogus":1"#);
    assert!(parse_task_file(&unknown_field).is_err());
    let missing_tool = ok.replace(r#"{"tool":"x"}"#, r#"{"tool":"y"}"#);
    assert!(parse_task_file(&missing_tool).unwrap_err().contains("not allowed"));
    let empty_expect = ok.replace(r#""expect":{"calls":[{"tool":"x"}]}"#, r#""expect":{}"#);
    assert!(parse_task_file(&empty_expect).unwrap_err().contains("no check"));
    let zero_turns = ok.replace(r#""id":"a""#, r#""id":"a","max_turns":0"#);
    assert!(parse_task_file(&zero_turns).is_err());
    let wrong_toolset = ok.replace(r#""id":"a""#, r#""id":"a","toolset":"other""#);
    assert!(parse_task_file(&wrong_toolset).is_err());
    let task = r#"{"id":"a","prompt":"p","allowed_tools":[{"name":"x","effect":{"kind":"reply","text":"r"}}],"expect":{"calls":[{"tool":"x"}]}}"#;
    let duplicate = format!(r#"{{"toolset":"t","tasks":[{task},{task}]}}"#);
    assert!(parse_task_file(&duplicate).unwrap_err().contains("duplicate"));
    assert!(parse_task_file("not json").is_err());
}

#[tokio::test]
async fn perfect_mock_passes_every_seed_task_and_qualifies() {
    let tasks = seed_tasks();
    let perfect = MockModel::new("mock-perfect", MockMode::Perfect);
    let matrix = run_matrix(&[&perfect], &tasks, 3, Thresholds::default()).await;
    assert_eq!(matrix.cells.len(), 5);
    for cell in &matrix.cells {
        assert_eq!((cell.tasks, cell.runs, cell.successes), (3, 3, 9), "{}", cell.toolset);
        assert!(cell.success_rate > 0.999 && cell.invalid_rate == 0.0);
        assert!(cell.qualified, "{}", cell.toolset);
        assert_eq!(cell.outcome_classes.get("ok"), Some(&cell.calls));
    }
    let files = matrix.cell("mock-perfect", "files").expect("files cell");
    assert_eq!(files.calls, 12);
    assert!((files.calls_per_task - 12.0 / 9.0).abs() < 1e-9);
}

#[tokio::test]
async fn broken_mock_is_unqualified_with_all_calls_invalid() {
    let tasks = only(&seed_tasks(), "files");
    let broken = MockModel::new("mock-broken", MockMode::Broken);
    let matrix = run_matrix(&[&broken], &tasks, 3, Thresholds::default()).await;
    let cell = &matrix.cells[0];
    assert_eq!(cell.successes, 0);
    assert!((cell.invalid_rate - 1.0).abs() < 1e-9);
    assert_eq!(cell.outcome_classes.get("unknown_tool"), Some(&cell.calls));
    assert!(!cell.qualified);
}

#[tokio::test]
async fn wrong_values_fail_tasks_without_invalid_calls() {
    let tasks = only(&seed_tasks(), "files");
    let wrong = MockModel::new("mock-wrong", MockMode::WrongArgs);
    let matrix = run_matrix(&[&wrong], &tasks, 3, Thresholds::default()).await;
    let cell = &matrix.cells[0];
    assert_eq!(cell.successes, 0);
    assert_eq!(cell.invalid_calls, 0);
    assert!(!cell.qualified, "invalid share alone must not qualify a model");
}

#[tokio::test]
async fn flaky_mock_gets_per_toolset_flags_from_the_formula() {
    let tasks = seed_tasks();
    let flaky = MockModel::new("mock-flaky", MockMode::FailEvery(10));
    let matrix = run_matrix(&[&flaky], &tasks, 3, Thresholds::default()).await;
    // Task runs are numbered task-major in load order (code_intel, files, git,
    // plan, shell); runs 9, 19, 29, 39 are broken, one in each toolset but the first.
    assert!(matrix.cell("mock-flaky", "code_intel").unwrap().qualified);
    for toolset in ["files", "git", "plan", "shell"] {
        let cell = matrix.cell("mock-flaky", toolset).unwrap();
        assert_eq!(cell.successes, 8, "{toolset}");
        assert!(!cell.qualified, "{toolset}");
    }
    // Relaxed thresholds flip the flag without re-running anything.
    let relaxed = Thresholds { min_success: 0.85, max_invalid: 0.25 };
    let again = run_matrix(&[&flaky], &tasks, 3, relaxed).await;
    assert!(again.cells.iter().all(|c| c.qualified));
}

fn outcome(success: bool, class: Class) -> RunOutcome {
    RunOutcome {
        task_id: "t".into(),
        toolset: "s".into(),
        run: 0,
        success,
        turns: 1,
        calls: vec![CallRecord { step: 1, tool: "x".into(), class }],
        failure: None,
    }
}

fn matrix_of(successes: usize, invalid: usize, thresholds: Thresholds) -> Matrix {
    let runs = (0..30)
        .map(|i| {
            let class = if i < invalid { Class::InvalidJson } else { Class::Ok };
            outcome(i >= 30 - successes, class)
        })
        .collect();
    build_matrix(vec![("m".into(), runs)], 3, thresholds)
}

#[test]
fn qualification_formula_boundaries() {
    let default = Thresholds::default();
    // Both bounds are inclusive: 27/30 is exactly 0.90 successes.
    assert!(matrix_of(27, 1, default).cells[0].qualified);
    assert!(!matrix_of(26, 0, default).cells[0].qualified);
    assert!(!matrix_of(30, 2, default).cells[0].qualified);
    assert!(matrix_of(30, 0, default).cells[0].qualified);
    let relaxed = Thresholds { min_success: 0.8, max_invalid: 0.10 };
    assert!(matrix_of(26, 2, relaxed).cells[0].qualified);
    let strict = Thresholds { min_success: 1.0, max_invalid: 0.0 };
    assert!(!matrix_of(29, 0, strict).cells[0].qualified);
    assert!(matrix_of(30, 0, strict).cells[0].qualified);
    assert_eq!(matrix_of(27, 1, default).cells[0].tasks, 10);
}

#[test]
fn reports_carry_the_qualified_flag_in_json_and_markdown() {
    let matrix = matrix_of(30, 0, Thresholds::default());
    let json: Value = serde_json::from_str(&matrix.to_json()).expect("json");
    assert_eq!(json["cells"][0]["qualified"], true);
    assert_eq!(json["cells"][0]["outcome_classes"]["ok"], 30);
    assert_eq!(json["thresholds"]["min_success"], 0.9);
    let markdown = matrix.to_markdown();
    assert!(markdown.contains("| m | s | 10x3 | 1.00 | 1.00 | 0.00 | yes | ok=30 |"), "{markdown}");
}

struct Announcer;
#[async_trait]
impl Model for Announcer {
    fn name(&self) -> &str {
        "announcer"
    }
    async fn turn(&self, _t: &Task, _c: &RunCtx, _m: &[Value]) -> Result<ModelTurn, String> {
        Ok(ModelTurn { text: "Let me read the file first.".into(), calls: vec![] })
    }
}

struct Looper;
#[async_trait]
impl Model for Looper {
    fn name(&self) -> &str {
        "looper"
    }
    async fn turn(&self, _t: &Task, c: &RunCtx, _m: &[Value]) -> Result<ModelTurn, String> {
        let call = ModelCall {
            id: format!("c{}", c.turn),
            name: "read_file".into(),
            arguments: r#"{"file_path":"missing.txt"}"#.into(),
        };
        Ok(ModelTurn { text: String::new(), calls: vec![call] })
    }
}

#[tokio::test]
async fn announced_without_call_repeats_and_turn_limit_are_recorded() {
    let task = only(&seed_tasks(), "files").remove(0);
    let run = run_task(&Announcer, &task, 0, 0).await;
    assert!(!run.success);
    assert_eq!(run.calls.len(), 1);
    assert_eq!(run.calls[0].class, Class::AnnouncedNoCall);

    let run = run_task(&Looper, &task, 0, 0).await;
    assert!(!run.success);
    assert_eq!(run.turns, task.max_turns);
    // The run record's rule: the second identical failure only fails again,
    // the third and later are repeats.
    assert!(run.calls.len() >= 3);
    assert_eq!(run.calls[0].class, Class::Precondition);
    assert_eq!(run.calls[1].class, Class::Precondition);
    assert!(run.calls[2..].iter().all(|c| c.class == Class::RepeatIdentical));
    assert!(run.failure.unwrap().contains("turns"));
}

#[test]
fn live_endpoint_is_refused_without_the_flag_and_never_shows_the_key() {
    let config = || LiveConfig {
        base_url: "http://127.0.0.1:9/v1".into(),
        model: "m".into(),
        key: Secret::new("sk-super-secret"),
    };
    let refused = live_model(config(), false).err().expect("refused");
    assert!(refused.contains("--live"));
    assert!(!refused.contains("sk-super-secret"));
    assert!(!format!("{:?}", config()).contains("sk-super-secret"));
    // Building the model does not touch the network.
    assert!(live_model(config(), true).is_ok());
}
