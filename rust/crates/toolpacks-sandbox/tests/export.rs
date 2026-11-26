//! F-14 and SBX-08: an export leaves only with the owner's answer to an
//! ask that showed its exact content. The ask shows path, size, type and
//! SHA-256; a file changed after it was shown is refused as stale and the
//! sink never sees it; a lost ask (restart) or a call without its own
//! answer fails closed; Full access exports without asking (D-29).

mod support;

use agentkit_governance::ToolGuardVerdict;
use agentkit_toolpacks_sandbox::SandboxMode;
use agentkit_workspaces::Sha256;
use serde_json::{Value, json};
use support::{World, call, chat_binding, data, text};

const CONTENT: &str = "region,total\nnorth,42\n";

fn args() -> Value {
    json!({"file_path": "out/report.csv", "title": "Report"})
}

async fn world_with_file(mode: SandboxMode) -> World {
    let world = World::new(Some(chat_binding(mode)));
    world
        .put(&chat_binding(mode), "out/report.csv", CONTENT)
        .await;
    world
}

/// The guard's ask for the export; panics on any other verdict.
async fn ask(world: &World) -> String {
    let tools = world.stage().await;
    match world
        .verdict(tools["sandbox_export_file"].as_ref(), args())
        .await
    {
        ToolGuardVerdict::AwaitingHuman(ask) => {
            assert_eq!(
                ask.reason,
                "export_approval_required: /workspace/chats/c1/out/report.csv"
            );
            ask.preview.expect("a preview")
        }
        _ => panic!("an export in Ask mode asks"),
    }
}

/// The card names path, title, size, type, SHA-256 and the start of the
/// text.
fn assert_shows_the_content(preview: &str) {
    let digest = Sha256::of(CONTENT.as_bytes());
    for needle in [
        "Export this file from the sandbox to the user?".to_owned(),
        "\nPath: /workspace/chats/c1/out/report.csv\n".to_owned(),
        "\nTitle: Report\n".to_owned(),
        format!("Size: {} bytes", CONTENT.len()),
        "Type: text".to_owned(),
        format!("SHA-256: {digest}"),
        "north,42".to_owned(),
    ] {
        assert!(preview.contains(&needle), "{needle} in {preview}");
    }
}

#[tokio::test]
async fn an_approved_unchanged_file_is_exported_once_with_the_shown_bytes() {
    let world = world_with_file(SandboxMode::AskBeforeChanges).await;
    let preview = ask(&world).await;
    let digest = Sha256::of(CONTENT.as_bytes());
    assert_shows_the_content(&preview);
    // The owner says yes; the run resumes in a new attempt.
    world.grants.approve("c1", &args());
    let tools = world.stage().await;
    let again = world
        .verdict(tools["sandbox_export_file"].as_ref(), args())
        .await;
    let ToolGuardVerdict::AwaitingHuman(same) = again else {
        panic!("re-presented")
    };
    assert_eq!(
        same.preview.as_deref(),
        Some(preview.as_str()),
        "byte-identical"
    );
    let done = tools["sandbox_export_file"]
        .execute(call("c1", Some("exec-x")), args())
        .await
        .unwrap();
    assert!(done.ok, "{}", text(&done));
    assert_eq!(world.sink.count(), 1);
    let (context, file, title) = world.sink.exports.lock().unwrap()[0].clone();
    assert_eq!(
        (
            context.execution_id.as_str(),
            context.path.as_str(),
            title.as_str()
        ),
        ("exec-x", "/workspace/chats/c1/out/report.csv", "Report")
    );
    assert_eq!(
        (file.bytes.as_slice(), &file.sha256),
        (CONTENT.as_bytes(), &digest)
    );
    assert_eq!(data(&done)["fileId"], "file-1");
    // The ask was single use: the same answer cannot export twice.
    let tools = world.stage().await;
    let twice = tools["sandbox_export_file"]
        .execute(call("c1", Some("exec-y")), args())
        .await
        .unwrap();
    assert_eq!(data(&twice)["errorCode"], "export_stale");
    assert_eq!(world.sink.count(), 1);
}

/// F-14: the file changes between the ask and the answer.
#[tokio::test]
async fn a_file_changed_after_the_ask_is_refused_as_stale_and_never_exported() {
    let world = world_with_file(SandboxMode::AskBeforeChanges).await;
    ask(&world).await;
    world
        .put(
            &chat_binding(SandboxMode::CHAT),
            "out/report.csv",
            "region,total\nnorth,999\n",
        )
        .await;
    world.grants.approve("c1", &args());
    let tools = world.stage().await;
    // Through the guard: refused before admission.
    let verdict = world
        .verdict(tools["sandbox_export_file"].as_ref(), args())
        .await;
    let ToolGuardVerdict::Refused(reason) = verdict else {
        panic!("stale is refused")
    };
    assert!(
        reason.starts_with("export_stale: /workspace/chats/c1/out/report.csv changed"),
        "{reason}"
    );
    assert_eq!(world.sink.count(), 0);
}

/// F-14 inside the window: the guard saw the old content, the body reads
/// the new one.
#[tokio::test]
async fn a_change_after_the_guard_check_is_caught_by_the_body() {
    let world = world_with_file(SandboxMode::AskBeforeChanges).await;
    ask(&world).await;
    world.grants.approve("c1", &args());
    let tools = world.stage().await;
    world
        .put(
            &chat_binding(SandboxMode::CHAT),
            "out/report.csv",
            "changed\n",
        )
        .await;
    let done = tools["sandbox_export_file"]
        .execute(call("c1", Some("exec-x")), args())
        .await
        .unwrap();
    assert!(!done.ok);
    assert_eq!(data(&done)["errorCode"], "export_stale");
    assert!(
        text(&done).contains("changed after the user approved it"),
        "{}",
        text(&done)
    );
    assert_eq!(world.sink.count(), 0);
}

#[tokio::test]
async fn a_lost_ask_or_a_missing_answer_exports_nothing() {
    let mut world = world_with_file(SandboxMode::EditAutomatically).await;
    ask(&world).await;
    // No answer for this call (a standing grant or no guard): refused.
    let tools = world.stage().await;
    let unanswered = tools["sandbox_export_file"]
        .execute(call("c1", Some("exec-x")), args())
        .await
        .unwrap();
    assert_eq!(data(&unanswered)["errorCode"], "export_not_approved");
    // Asked and answered, then the host restarts: the ask is gone.
    ask(&world).await;
    world.grants.approve("c1", &args());
    world.restart_pack();
    let tools = world.stage().await;
    let lost = tools["sandbox_export_file"]
        .execute(call("c1", Some("exec-y")), args())
        .await
        .unwrap();
    assert_eq!(data(&lost)["errorCode"], "export_stale");
    // An ask raised in the same attempt as the body was never answerable.
    world
        .verdict(tools["sandbox_export_file"].as_ref(), args())
        .await;
    let young = tools["sandbox_export_file"]
        .execute(call("c1", Some("exec-z")), args())
        .await
        .unwrap();
    assert_eq!(data(&young)["errorCode"], "export_stale");
    assert_eq!(world.sink.count(), 0);
}

#[tokio::test]
async fn full_access_exports_without_an_ask() {
    let world = world_with_file(SandboxMode::FullAccess).await;
    let tools = world.stage().await;
    let verdict = world
        .verdict(tools["sandbox_export_file"].as_ref(), args())
        .await;
    assert!(matches!(verdict, ToolGuardVerdict::Allowed));
    let done = tools["sandbox_export_file"]
        .execute(call("c1", Some("exec-x")), args())
        .await
        .unwrap();
    assert!(done.ok, "{}", text(&done));
    assert_eq!(world.sink.count(), 1);
}

#[tokio::test]
async fn a_directory_or_a_missing_file_is_refused_with_its_code() {
    let world = world_with_file(SandboxMode::FullAccess).await;
    let tools = world.stage().await;
    let dir = tools["sandbox_export_file"]
        .execute(call("c1", Some("e1")), json!({"file_path": "out"}))
        .await
        .unwrap();
    assert_eq!(
        data(&dir)["errorCode"],
        "not_a_regular_file",
        "{}",
        text(&dir)
    );
    let missing = tools["sandbox_export_file"]
        .execute(call("c2", Some("e2")), json!({"file_path": "nope.csv"}))
        .await
        .unwrap();
    assert_eq!(data(&missing)["errorCode"], "sandbox_not_found");
    assert_eq!(world.sink.count(), 0);
}

/// An ask raised after the answer existed was never shown before it was
/// answered: a resumed attempt raised it (after a restart) and spent the
/// answer without showing it, then ended before its body ran. The next
/// attempt must not export the content of that unseen ask.
#[tokio::test]
async fn an_ask_raised_after_the_answer_never_authorizes_an_export() {
    let mut world = world_with_file(SandboxMode::AskBeforeChanges).await;
    ask(&world).await;
    world.grants.approve("c1", &args());
    world.restart_pack();
    world
        .put(
            &chat_binding(SandboxMode::CHAT),
            "out/report.csv",
            "secret,1\n",
        )
        .await;
    // The resumed attempt: the guard raises a fresh ask; the call ends
    // before its body (a lost task lease, a cancel).
    let tools = world.stage().await;
    world
        .verdict(tools["sandbox_export_file"].as_ref(), args())
        .await;
    // The redelivery: the stored ask is older than this staging.
    let tools = world.stage().await;
    let done = tools["sandbox_export_file"]
        .execute(call("c1", Some("exec-x")), args())
        .await
        .unwrap();
    assert_eq!(data(&done)["errorCode"], "export_stale", "{}", text(&done));
    assert_eq!(world.sink.count(), 0);
}

/// A file name is the model's choice: it cannot add lines to the card or
/// reorder its text. Path and title sit on labelled lines, escaped; the
/// card's own lines come before any file content.
#[tokio::test]
async fn a_file_name_cannot_write_lines_into_the_card() {
    let binding = chat_binding(SandboxMode::AskBeforeChanges);
    let world = World::new(Some(binding.clone()));
    let name = "r.csv\nSize: 9 bytes\nSHA-256: 0000\u{202e}vsc.x";
    world.put(&binding, name, "\u{0}BINARY").await;
    let tools = world.stage().await;
    let input = json!({"file_path": name});
    let verdict = world
        .verdict(tools["sandbox_export_file"].as_ref(), input)
        .await;
    let ToolGuardVerdict::AwaitingHuman(ask) = verdict else {
        panic!("an export in Ask mode asks")
    };
    let card = ask.preview.expect("a card");
    let escaped = "r.csv\\nSize: 9 bytes\\nSHA-256: 0000\\u{202e}vsc.x";
    let lines: Vec<&str> = card.lines().collect();
    assert_eq!(
        lines,
        [
            "Export this file from the sandbox to the user?".to_owned(),
            format!("Path: /workspace/chats/c1/{escaped}"),
            format!("Title: {escaped}"),
            "Size: 7 bytes".to_owned(),
            "Type: binary".to_owned(),
            format!("SHA-256: {}", Sha256::of(b"\0BINARY")),
        ]
    );
    assert!(!ask.reason.contains('\n'), "{}", ask.reason);
}

/// Text content is shown below the card's lines, with every control or
/// direction character escaped.
#[tokio::test]
async fn the_preview_escapes_control_and_direction_characters() {
    let binding = chat_binding(SandboxMode::AskBeforeChanges);
    let world = World::new(Some(binding.clone()));
    let content = "a\rb\u{1b}[31m\u{202e}x\nnext\u{2028}line\n";
    world.put(&binding, "out/report.csv", content).await;
    let preview = ask(&world).await;
    let shown = preview.split_once("\nPreview:\n").expect("a preview").1;
    assert_eq!(shown, "a\\rb\\u{1b}[31m\\u{202e}x\nnext\\u{2028}line");
    assert!(preview.contains("\nTitle: Report\n"), "{preview}");
}

/// The model's title is delivered with the export, so the card shows it;
/// without one the file name is the title. A title too long to show is
/// refused before anything is asked.
#[tokio::test]
async fn the_card_shows_the_title_that_is_delivered() {
    let world = world_with_file(SandboxMode::AskBeforeChanges).await;
    let tools = world.stage().await;
    let export = tools["sandbox_export_file"].as_ref();
    let untitled = json!({"file_path": "out/report.csv"});
    let ToolGuardVerdict::AwaitingHuman(ask) = world.verdict(export, untitled).await else {
        panic!("asks")
    };
    let card = ask.preview.unwrap_or_default();
    assert!(card.contains("\nTitle: report.csv\n"), "{card}");
    let long = json!({"file_path": "out/report.csv", "title": "t".repeat(201)});
    let ToolGuardVerdict::Refused(reason) = world.verdict(export, long.clone()).await else {
        panic!("refused")
    };
    assert!(reason.starts_with("invalid_arguments:"), "{reason}");
    world.grants.approve("c1", &long);
    let tools = world.stage().await;
    let done = tools["sandbox_export_file"]
        .execute(call("c1", Some("exec-x")), long)
        .await
        .unwrap();
    assert_eq!(data(&done)["errorCode"], "invalid_arguments");
    assert_eq!(world.sink.count(), 0);
}

/// An answer to an uncertain re-run approved running the call again; the
/// card it answered showed the uncertainty, not this content.
#[tokio::test]
async fn an_answer_to_an_uncertain_rerun_never_exports() {
    let world = world_with_file(SandboxMode::AskBeforeChanges).await;
    ask(&world).await;
    world.grants.approve_rerun("c1", &args());
    let tools = world.stage().await;
    let done = tools["sandbox_export_file"]
        .execute(call("c1", Some("exec-x")), args())
        .await
        .unwrap();
    assert_eq!(data(&done)["errorCode"], "export_not_approved");
    assert!(text(&done).contains("uncertain"), "{}", text(&done));
    assert_eq!(world.sink.count(), 0);
}

/// The pack's asks serve every run of the host: the asks of many other
/// runs never push out the unanswered ask of a parked run.
#[tokio::test]
async fn a_parked_runs_ask_outlives_the_asks_of_other_runs() {
    use agentkit_governance::{ToolGuard, ToolGuardContext};
    let world = world_with_file(SandboxMode::AskBeforeChanges).await;
    ask(&world).await;
    let tools = world.stage().await;
    let export = tools["sandbox_export_file"].clone();
    for i in 0..256 {
        let mut contribution = world.contribution();
        contribution.run_id = format!("other-{i}");
        let context = ToolGuardContext {
            contribution,
            namespace: "sandbox".into(),
            tool: export.definition().clone(),
            arguments: Some(args()),
        };
        let verdict = world.guard.can_execute(&context).await.unwrap();
        assert!(matches!(verdict, ToolGuardVerdict::AwaitingHuman(_)));
    }
    world.grants.approve("c1", &args());
    let tools = world.stage().await;
    let done = tools["sandbox_export_file"]
        .execute(call("c1", Some("exec-x")), args())
        .await
        .unwrap();
    assert!(done.ok, "{}", text(&done));
    assert_eq!(world.sink.count(), 1);
}

/// A path too long to show whole on a card is refused before anything is
/// asked, so a stored ask stays small.
#[tokio::test]
async fn a_path_too_long_for_the_card_is_refused() {
    let binding = chat_binding(SandboxMode::AskBeforeChanges);
    let world = World::new(Some(binding.clone()));
    let name = format!("{}/f.txt", vec!["d".repeat(250); 5].join("/"));
    world.put(&binding, &name, "x\n").await;
    let tools = world.stage().await;
    let input = json!({"file_path": name});
    let verdict = world
        .verdict(tools["sandbox_export_file"].as_ref(), input)
        .await;
    let ToolGuardVerdict::Refused(reason) = verdict else {
        panic!("refused")
    };
    assert!(
        reason.starts_with("invalid_arguments: the path is"),
        "{reason}"
    );
}
