//! Importing a TypeScript adapters-sqlite (schema v10, `application_id` 0)
//! database into a freshly opened Rust store.
//!
//! The source is built with raw SQL, copying the TypeScript adapter's own DDL
//! column for column, and every assertion goes back out through the Rust
//! store's typed read paths — what an import is worth is what
//! [`SqliteStore::get_chat`], [`SqliteStore::messages`], [`SqliteStore::get_task`]
//! and friends read afterwards.

#![cfg(feature = "ts-import")]

use agentkit_store::*;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::path::Path;

const T0: &str = "2026-01-02T03:04:05.678Z";
const T1: &str = "2026-02-03T04:05:06.789Z";
const T2: &str = "2026-03-04T05:06:07.890Z";
const T3: &str = "2026-04-05T06:07:08.901Z";
const T4: &str = "2026-05-06T07:08:09.012Z";
const T0_MS: i64 = 1_767_323_045_678;
const T1_MS: i64 = 1_770_091_506_789;
const T2_MS: i64 = 1_772_600_767_890;
const T3_MS: i64 = 1_775_369_228_901;
const T4_MS: i64 = 1_778_051_289_012;

/// The TypeScript layout, transcribed from `SCHEMA_V8` plus the v9
/// (`provider_run_scopes` / `provider_continuations`) and v10
/// (`messages.archived`) migrations — columns and types exactly as shipped,
/// indexes and the FTS triggers excepted.
const TS_SCHEMA: &str = r#"
CREATE TABLE chats (
  id TEXT PRIMARY KEY, title TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
  metadata TEXT NOT NULL DEFAULT '{}', archived INTEGER NOT NULL DEFAULT 0);
CREATE TABLE messages (
  id TEXT PRIMARY KEY,
  chat_id TEXT NOT NULL REFERENCES chats(id),
  run_id TEXT, role TEXT NOT NULL,
  content TEXT NOT NULL, content_format TEXT NOT NULL DEFAULT 'text',
  order_key INTEGER NOT NULL, tool_call_id TEXT, tool_calls TEXT, model_result_json TEXT,
  parent_message_id TEXT REFERENCES messages(id),
  depth INTEGER NOT NULL DEFAULT 0, branch_index INTEGER NOT NULL DEFAULT 0,
  active INTEGER NOT NULL DEFAULT 1, metadata TEXT NOT NULL DEFAULT '{}',
  created_at TEXT NOT NULL, archived INTEGER NOT NULL DEFAULT 0,
  UNIQUE (chat_id, order_key));
CREATE TABLE tasks (
  task_id TEXT PRIMARY KEY, kind TEXT NOT NULL, scope_id TEXT NOT NULL, status TEXT NOT NULL,
  priority INTEGER NOT NULL DEFAULT 0, enqueued_at TEXT NOT NULL, available_at TEXT NOT NULL,
  started_at TEXT, finished_at TEXT, payload TEXT NOT NULL, parent_task_id TEXT,
  depends_on TEXT, progress TEXT, error TEXT,
  attempt_count INTEGER NOT NULL DEFAULT 0, poison_count INTEGER NOT NULL DEFAULT 0,
  dead_lettered_at TEXT, dead_letter_reason TEXT);
CREATE TABLE task_attempts (
  attempt_id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(task_id),
  attempt_number INTEGER NOT NULL, status TEXT NOT NULL, owner_id TEXT NOT NULL,
  started_at TEXT NOT NULL, ended_at TEXT, error TEXT);
CREATE TABLE leases (
  task_id TEXT PRIMARY KEY REFERENCES tasks(task_id),
  attempt_id TEXT NOT NULL, owner_id TEXT NOT NULL, lease_token TEXT NOT NULL UNIQUE,
  fencing_token INTEGER NOT NULL, expires_at TEXT NOT NULL);
CREATE TABLE task_events (
  task_id TEXT NOT NULL, seq INTEGER NOT NULL, event_id TEXT NOT NULL UNIQUE,
  attempt_id TEXT, type TEXT NOT NULL, timestamp TEXT NOT NULL, payload TEXT NOT NULL,
  PRIMARY KEY (task_id, seq));
CREATE TABLE outbox (
  id TEXT PRIMARY KEY, topic TEXT NOT NULL, run_id TEXT, payload TEXT NOT NULL,
  created_at TEXT NOT NULL, available_at TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
  published_at TEXT, last_error TEXT);
CREATE TABLE proposals (
  id TEXT PRIMARY KEY, chat_id TEXT NOT NULL, run_id TEXT, scope_key TEXT NOT NULL,
  action_id TEXT, tool_name TEXT NOT NULL, kind TEXT NOT NULL, risk TEXT NOT NULL,
  status TEXT NOT NULL, envelope TEXT NOT NULL, operations TEXT NOT NULL,
  warnings TEXT NOT NULL, truncated INTEGER NOT NULL, revision_at_create TEXT,
  operation_id TEXT, decision TEXT, reason TEXT, created_at TEXT NOT NULL, decided_at TEXT,
  claimed_at TEXT, applied_at TEXT);
CREATE TABLE proposal_outcomes (
  operation_id TEXT PRIMARY KEY, status TEXT NOT NULL, applied_ops INTEGER NOT NULL,
  failed_ops TEXT NOT NULL, result_json TEXT, revision TEXT);
CREATE TABLE providers (
  id TEXT PRIMARY KEY, label TEXT NOT NULL, kind TEXT NOT NULL, base_url TEXT NOT NULL,
  api_key TEXT, default_model TEXT NOT NULL, enabled INTEGER NOT NULL, extra_headers TEXT,
  metadata TEXT);
CREATE TABLE provider_models (
  provider_id TEXT NOT NULL REFERENCES providers(id), model_id TEXT NOT NULL,
  display_name TEXT, context_window_tokens INTEGER, supports_tool_calling INTEGER,
  fetched_at TEXT NOT NULL, PRIMARY KEY (provider_id, model_id));
CREATE TABLE provider_capabilities (
  provider_id TEXT PRIMARY KEY REFERENCES providers(id), streaming INTEGER NOT NULL,
  tool_calling INTEGER NOT NULL, model_list INTEGER NOT NULL, vision INTEGER,
  json_mode INTEGER, max_context_tokens INTEGER, checked_at TEXT, warning TEXT);
CREATE TABLE settings (
  id INTEGER PRIMARY KEY CHECK (id = 1), default_provider_id TEXT, default_model TEXT,
  context_size_preference TEXT NOT NULL, write_policy_mode TEXT NOT NULL,
  allow_raw_tool_data INTEGER NOT NULL, max_tool_iterations INTEGER,
  tool_calling_mode TEXT NOT NULL DEFAULT 'auto', metadata TEXT NOT NULL DEFAULT '{}');
CREATE TABLE mcp_servers (
  id TEXT PRIMARY KEY, alias TEXT NOT NULL UNIQUE, transport TEXT NOT NULL, secret_refs TEXT,
  enabled INTEGER, tool_aliases TEXT, resilience TEXT,
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE fencing_counter (id INTEGER PRIMARY KEY CHECK (id = 1), value INTEGER NOT NULL DEFAULT 0);
CREATE TABLE provider_run_scopes (
  run_id TEXT PRIMARY KEY REFERENCES tasks(task_id) ON DELETE CASCADE,
  scope_json TEXT NOT NULL, state_required INTEGER NOT NULL DEFAULT 0);
CREATE TABLE provider_continuations (
  run_id TEXT PRIMARY KEY REFERENCES tasks(task_id) ON DELETE CASCADE,
  anchor_message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
  chat_id TEXT NOT NULL REFERENCES chats(id) ON DELETE CASCADE, state_json TEXT NOT NULL);
CREATE TABLE agent_inputs (
  input_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, kind TEXT NOT NULL,
  status TEXT NOT NULL, content TEXT NOT NULL, content_hash TEXT NOT NULL,
  input_order INTEGER NOT NULL, target_task_id TEXT, on_settled TEXT, attempt_id TEXT,
  tool_call_id TEXT, expires_at TEXT, applied_task_id TEXT, applied_message_id TEXT,
  answer_id TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE context_summaries (
  summary_id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
  covers_through_message_id TEXT NOT NULL, first_kept_message_id TEXT NOT NULL,
  summary TEXT NOT NULL, source_hash TEXT NOT NULL, tokens_before INTEGER NOT NULL,
  tokens_after INTEGER NOT NULL, provider_id TEXT, model TEXT NOT NULL,
  summarizer_revision TEXT NOT NULL, details TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE VIRTUAL TABLE message_search USING fts5(body, content='');
"#;

/// The scope a real `Responses` continuation binds, and its state.
const SCOPE: &str = r#"{"providerId":"openai","protocol":"responses","connectionId":"conn-1","generation":1,"chatId":"chat-1","branchId":"branch-1","model":"gpt-5"}"#;
const STATE: &str = r#"{"version":1,"scope":{"providerId":"openai","protocol":"responses","connectionId":"conn-1","generation":1,"chatId":"chat-1","branchId":"branch-1","model":"gpt-5"},"messageCount":3,"messageDigest":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","inputItems":[{"role":"user"}]}"#;

/// A TypeScript assistant-store file with a branched chat tree, a running task
/// with two attempts, a lease, events, a settled proposal, a provider, MCP and
/// assistant settings, and one table the Rust store cannot represent.
fn ts_database(path: &Path) {
    let db = Connection::open(path).expect("open the fixture");
    db.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    db.execute_batch(TS_SCHEMA).unwrap();
    db.pragma_update(None, "user_version", 10).unwrap();
    db.execute_batch(&format!(
        r#"
INSERT INTO chats VALUES ('chat-1','Working','{T0}','{T1}','{{"project":"OneMind"}}',0);
INSERT INTO chats VALUES ('chat-2',NULL,'{T1}','{T1}','{{}}',1);
INSERT INTO messages (id,chat_id,run_id,role,content,content_format,order_key,tool_call_id,
    tool_calls,model_result_json,parent_message_id,depth,branch_index,active,metadata,
    created_at,archived) VALUES
  ('m1','chat-1',NULL,'user','Hello there','text',1,NULL,NULL,NULL,NULL,0,0,1,
   '{{"tag":"first"}}','{T0}',0),
  ('m2','chat-1','run-1','assistant','[{{"type":"text","text":"Hi"}}]','parts',2,NULL,
   '[{{"id":"call-1","type":"function","function":{{"name":"read_file","arguments":"{{\"path\":\"a.txt\"}}"}}}}]',
   '{{"finishReason":"stop"}}','m1',1,0,1,'{{"tokens":10}}','{T1}',0),
  ('m3','chat-1','run-1','tool','ok','text',3,'call-1',NULL,NULL,'m2',2,0,1,'{{}}','{T1}',0),
  ('m4','chat-1',NULL,'user','Other branch','text',4,NULL,NULL,NULL,'m1',1,1,1,'{{}}','{T2}',0),
  ('m5','chat-1',NULL,'assistant','Stale','text',5,NULL,NULL,NULL,'m4',2,0,1,'{{}}','{T2}',1),
  ('m7','chat-1',NULL,'assistant','Abandoned branch','text',6,NULL,NULL,NULL,'m1',1,2,0,'{{}}','{T2}',0),
  ('m6','chat-2',NULL,'user','Old chat','text',1,NULL,NULL,NULL,NULL,0,0,1,'{{}}','{T3}',0);
INSERT INTO tasks (task_id,kind,scope_id,status,priority,enqueued_at,available_at,started_at,
    finished_at,payload,parent_task_id,depends_on,progress,error,attempt_count,poison_count,
    dead_lettered_at,dead_letter_reason) VALUES
  ('run-1','chat.turn','chat-1','running',5,'{T0}','{T0}','{T1}',NULL,
   '{{"chatId":"chat-1","prompt":"hi"}}',NULL,'["dep-1"]','{{"step":2}}',NULL,2,1,NULL,NULL),
  ('dep-1','index','scope-index','completed',0,'{T0}','{T0}','{T1}','{T2}',
   '{{"path":"."}}',NULL,NULL,NULL,NULL,1,0,NULL,NULL),
  ('dead-1','sweep','scope-sweep','cancelled',0,'{T0}','{T0}',NULL,'{T3}','{{}}',NULL,NULL,NULL,
   'boom',3,2,'{T3}','poisoned');
INSERT INTO task_attempts VALUES
  ('attempt-1','run-1',1,'completed','worker-1','{T0}','{T1}',NULL),
  ('attempt-2','run-1',2,'running','worker-1','{T1}',NULL,NULL);
INSERT INTO leases VALUES ('run-1','attempt-2','worker-1','lease-1',7,'{T4}');
INSERT INTO task_events VALUES
  ('run-1',0,'event-1','attempt-2','run.started','{T1}',
   '{{"type":"run.started","seq":0,"eventId":"event-1","timestamp":"{T1}","contractVersion":"1.0.0","attemptId":"attempt-2","data":{{"tokens":7}}}}'),
  ('run-1',1,'event-2',NULL,'run.token','{T2}',
   '{{"type":"run.token","seq":1,"eventId":"event-2","timestamp":"{T2}","contractVersion":"1.0.0","data":{{"text":"Hi"}}}}');
INSERT INTO outbox VALUES ('out-1','task.events','run-1','{{"seq":0}}','{T1}','{T1}',2,NULL,NULL);
INSERT INTO proposals VALUES
  ('prop-1','chat-1','run-1','scope-1','action-1','write_file','write','medium','applied',
   '{{"path":"a.txt"}}','[{{"opIndex":0}}]','["wide"]',0,'rev-9','op-1','{{"approved":true}}',
   NULL,'{T0}','{T1}','{T2}','{T3}');
INSERT INTO proposal_outcomes VALUES
  ('op-1','applied',1,'[{{"opIndex":1,"error":"denied"}}]','{{"ok":true}}','rev-10');
INSERT INTO providers VALUES ('openai','OpenAI','openai','https://api.example/v1','sk-test',
  'gpt-5',1,'{{"X-Trace":"on"}}','{{"tier":"pro"}}');
INSERT INTO provider_models VALUES
  ('openai','gpt-5','GPT-5',128000,1,'{T2}'),
  ('openai','gpt-mini',NULL,NULL,NULL,'{T2}');
INSERT INTO provider_capabilities VALUES ('openai',1,1,1,1,NULL,200000,'{T3}','rate limited');
INSERT INTO settings VALUES (1,'openai','gpt-5','small','auto_readonly_confirm_writes',0,24,
  'auto','{{"theme":"dark"}}');
INSERT INTO mcp_servers VALUES ('server-1','fs','{{"kind":"stdio","command":"npx"}}',
  '{{"TOKEN":"secret://x"}}',NULL,'{{"ls":"list"}}',NULL,'{T0}','{T1}');
INSERT INTO fencing_counter VALUES (1,7);
INSERT INTO agent_inputs VALUES ('input-1','chat-1','steer','pending','{{"text":"go"}}','hash',
  3,'run-1','follow_up',NULL,NULL,NULL,NULL,NULL,NULL,'{T0}','{T0}');
INSERT INTO agent_inputs VALUES ('input-2','chat-2','human_request','answered',
  '{{"question":"which?"}}','hash2',7,NULL,NULL,'a2',NULL,'{T3}',NULL,NULL,'answer-1',
  '{T0}','{T1}');
INSERT INTO context_summaries VALUES ('sum-1','chat-1','m2','m3','Earlier turns',
  'cafe',1200,300,'openai','gpt-5','rev-1',
  '{{"readFiles":["a.txt"],"modifiedFiles":[]}}','{T2}');
INSERT INTO provider_run_scopes VALUES ('run-1','{SCOPE}',0);
INSERT INTO provider_continuations VALUES ('run-1','m2','chat-1','{STATE}');
"#
    ))
    .expect("seed the fixture");
    drop(db);
}

fn options() -> ImportOptions {
    ImportOptions::default()
}

#[tokio::test]
async fn conversations_and_task_state_round_trip_through_the_typed_reads() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("ts.db");
    let destination = root.path().join("agentkit.db");
    ts_database(&source);
    let store = SqliteStore::open(&destination, 8).unwrap();
    let report = store
        .import_from_ts_schema(&source, &options())
        .await
        .unwrap();
    assert_eq!(
        (
            report.chats,
            report.messages,
            report.tasks,
            report.events,
            report.attempts
        ),
        (2, 7, 3, 2, 2),
        "{report:?}"
    );

    let chat = store.get_chat("chat-1").await.unwrap().unwrap();
    assert_eq!(chat.id, "chat-1");
    assert_eq!(chat.title.as_deref(), Some("Working"));
    assert_eq!(chat.metadata, json!({"project": "OneMind"}));
    assert!(!chat.archived);
    assert_eq!(chat.created_at.epoch_millis(), Some(T0_MS));
    assert_eq!(chat.updated_at.epoch_millis(), Some(T1_MS));
    assert!(store.get_chat("chat-2").await.unwrap().unwrap().archived);
    assert_eq!(
        store
            .list_chats(false)
            .await
            .unwrap()
            .iter()
            .map(|chat| chat.id.as_str())
            .collect::<Vec<_>>(),
        ["chat-1"]
    );

    // The active view, in tree order.
    assert_eq!(
        store
            .messages("chat-1", false)
            .await
            .unwrap()
            .iter()
            .map(|message| message.id.as_str())
            .collect::<Vec<_>>(),
        ["m1", "m2", "m3", "m4", "m5"]
    );
    // With inactive branches included, the abandoned branch shows up.
    assert_eq!(
        store
            .messages("chat-1", true)
            .await
            .unwrap()
            .iter()
            .map(|message| message.id.as_str())
            .collect::<Vec<_>>(),
        ["m1", "m2", "m3", "m4", "m5", "m7"]
    );
    let root_message = store.get_message("m1").await.unwrap().unwrap();
    assert_eq!(root_message.role, "user");
    assert_eq!(root_message.content, json!("Hello there"));
    assert_eq!(root_message.parent_message_id, None);
    assert_eq!(root_message.metadata, json!({"tag": "first"}));
    assert_eq!(root_message.created_at.epoch_millis(), Some(T0_MS));
    let answer = store.get_message("m2").await.unwrap().unwrap();
    assert_eq!(answer.role, "assistant");
    assert_eq!(answer.chat_id, "chat-1");
    assert_eq!(answer.run_id.as_deref(), Some("run-1"));
    assert_eq!(answer.content, json!([{"type": "text", "text": "Hi"}]));
    assert_eq!(answer.parent_message_id.as_deref(), Some("m1"));
    assert_eq!(answer.tool_call_id, None);
    assert_eq!(
        answer.tool_calls,
        Some(json!([{"id": "call-1", "type": "function", "function": {
            "name": "read_file", "arguments": "{\"path\":\"a.txt\"}" }}]))
    );
    assert_eq!(
        answer.model_result_json.as_deref(),
        Some(r#"{"finishReason":"stop"}"#)
    );
    assert_eq!(answer.metadata, json!({"tokens": 10}));
    assert_eq!(
        (answer.order_key, answer.depth, answer.branch_index),
        (2, 1, 0)
    );
    assert!(answer.active);
    assert!(!answer.archived);
    assert_eq!(answer.created_at.epoch_millis(), Some(T1_MS));
    let tool = store.get_message("m3").await.unwrap().unwrap();
    assert_eq!(
        (tool.role.as_str(), tool.tool_call_id.as_deref()),
        ("tool", Some("call-1"))
    );
    assert_eq!(tool.depth, 2);
    let branch = store.get_message("m4").await.unwrap().unwrap();
    assert_eq!(
        (branch.role.as_str(), branch.branch_index, branch.active),
        ("user", 1, true)
    );
    assert_eq!(branch.parent_message_id.as_deref(), Some("m1"));
    let abandoned = store.get_message("m7").await.unwrap().unwrap();
    assert_eq!((abandoned.branch_index, abandoned.active), (2, false));
    assert!(store.get_message("m5").await.unwrap().unwrap().archived);

    let task = store.get_task("run-1").await.unwrap().unwrap();
    assert_eq!(task.kind, "chat.turn");
    assert_eq!(task.scope_id, "chat-1");
    assert_eq!(task.status, TaskStatus::Running);
    assert_eq!(task.payload, json!({"chatId": "chat-1", "prompt": "hi"}));
    assert_eq!(task.priority, 5);
    assert_eq!(task.enqueued_at, T0_MS);
    assert_eq!(task.available_at, T0_MS);
    assert_eq!(task.started_at, Some(T1_MS));
    assert_eq!(task.finished_at, None);
    assert_eq!(task.depends_on, ["dep-1"]);
    assert_eq!(task.parent_task_id, None);
    assert_eq!(task.attempt_count, 2);
    assert_eq!(task.poison_count, 1);
    assert_eq!(task.progress, Some(json!({"step": 2})));
    assert_eq!(task.error, None);
    let settled = store.get_task("dep-1").await.unwrap().unwrap();
    assert_eq!(settled.status, TaskStatus::Completed);
    assert_eq!(settled.finished_at, Some(T2_MS));
    let dead = store.get_task("dead-1").await.unwrap().unwrap();
    assert_eq!(dead.status, TaskStatus::Cancelled);
    assert_eq!(dead.error.as_deref(), Some("boom"));
    assert_eq!(dead.dead_lettered_at, Some(T3_MS));
    assert_eq!(dead.dead_letter_reason.as_deref(), Some("poisoned"));
    assert_eq!(
        store
            .list_tasks(Some("chat-1".into()))
            .await
            .unwrap()
            .iter()
            .map(|task| task.task_id.as_str())
            .collect::<Vec<_>>(),
        ["run-1"]
    );

    assert_eq!(
        store.attempts("run-1").await.unwrap(),
        vec![
            AttemptRecord {
                attempt_id: "attempt-1".into(),
                task_id: "run-1".into(),
                attempt_number: 1,
                status: "completed".into(),
                owner_id: "worker-1".into(),
                started_at: T0_MS,
                ended_at: Some(T1_MS),
                error: None,
            },
            AttemptRecord {
                attempt_id: "attempt-2".into(),
                task_id: "run-1".into(),
                attempt_number: 2,
                status: "running".into(),
                owner_id: "worker-1".into(),
                started_at: T1_MS,
                ended_at: None,
                error: None,
            },
        ]
    );

    let events = store.events("run-1", None, 10).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!((events[0].seq, events[1].seq), (0.0, 1.0));
    assert_eq!(events[0].event["eventId"], json!("event-1"));
    assert_eq!(events[0].event["type"], json!("run.started"));
    assert_eq!(events[0].event["data"]["tokens"], json!(7));
    assert_eq!(events[0].created_at, T1_MS);
    assert_eq!(events[1].event["attemptId"], Value::Null);
    assert_eq!(store.events("run-1", Some(0), 10).await.unwrap().len(), 1);

    let lease = store.get_lease("run-1").await.unwrap().unwrap();
    assert_eq!(lease.task_id, "run-1");
    assert_eq!(lease.attempt_id, "attempt-2");
    assert_eq!(lease.owner_id, "worker-1");
    assert_eq!(lease.lease_token, "lease-1");
    assert_eq!(lease.fencing_token, 7);
    assert_eq!(lease.expires_at, T4_MS);
}

#[tokio::test]
async fn proposals_receipts_catalogue_and_settings_round_trip() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("ts.db");
    let destination = root.path().join("agentkit.db");
    ts_database(&source);
    let store = SqliteStore::open(&destination, 8).unwrap();
    let report = store
        .import_from_ts_schema(&source, &options())
        .await
        .unwrap();
    assert_eq!(
        (
            report.proposals,
            report.receipts,
            report.outbox,
            report.providers,
            report.mcp_servers
        ),
        (1, 1, 1, 1, 1),
        "{report:?}"
    );

    let proposal = store.get_proposal("prop-1").await.unwrap().unwrap();
    assert_eq!(proposal.chat_id, "chat-1");
    assert_eq!(proposal.run_id.as_deref(), Some("run-1"));
    assert_eq!(proposal.scope_key, "scope-1");
    assert_eq!(proposal.action_id.as_deref(), Some("action-1"));
    assert_eq!(proposal.tool_name, "write_file");
    assert_eq!(
        (proposal.kind.as_str(), proposal.risk.as_str()),
        ("write", "medium")
    );
    assert_eq!(proposal.status, ProposalStatus::Applied);
    assert_eq!(proposal.envelope, json!({"path": "a.txt"}));
    assert_eq!(proposal.operations, vec![json!({"opIndex": 0})]);
    assert_eq!(proposal.warnings, ["wide"]);
    assert!(!proposal.truncated);
    assert_eq!(proposal.revision_at_create.as_deref(), Some("rev-9"));
    assert_eq!(proposal.operation_id.as_deref(), Some("op-1"));
    assert_eq!(proposal.decision, Some(json!({"approved": true})));
    assert_eq!(proposal.reason, None);
    assert_eq!(
        (
            proposal.created_at,
            proposal.decided_at,
            proposal.claimed_at,
            proposal.applied_at
        ),
        (T0_MS, Some(T1_MS), Some(T2_MS), Some(T3_MS))
    );
    assert_eq!(
        store
            .proposal_by_action("scope-1", "action-1")
            .await
            .unwrap()
            .unwrap()
            .id,
        "prop-1"
    );
    assert_eq!(
        store
            .proposals(Some("chat-1".into()), Some(ProposalStatus::Applied))
            .await
            .unwrap()
            .len(),
        1
    );

    let receipt = store.receipt("op-1").await.unwrap().unwrap();
    assert_eq!(receipt.proposal_id, "prop-1");
    assert_eq!(receipt.operation_id, "op-1");
    assert_eq!(
        receipt.outcome,
        ApplyOutcome {
            status: "applied".into(),
            applied_ops: 1,
            failed_ops: vec![json!({"opIndex": 1, "error": "denied"})],
            result_json: Some(r#"{"ok":true}"#.into()),
            revision: Some("rev-10".into()),
        }
    );
    // The outcome row carries no instant; it inherits the applied stamp.
    assert_eq!(receipt.created_at, T3_MS);

    let outbox = store.outbox().await.unwrap();
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0].id, "out-1");
    assert_eq!(outbox[0].topic, "task.events");
    assert_eq!(outbox[0].run_id.as_deref(), Some("run-1"));
    assert_eq!(outbox[0].payload, json!({"seq": 0}));
    assert_eq!(
        (outbox[0].created_at, outbox[0].available_at),
        (T1_MS, T1_MS)
    );
    assert_eq!(outbox[0].attempts, 2);
    assert_eq!(outbox[0].published_at, None);
    assert_eq!(outbox[0].last_error, None);
    assert_eq!(outbox[0].claim_token, None);

    let providers = store.list_providers().await.unwrap();
    assert_eq!(providers.len(), 1);
    let openai = &providers[0];
    assert_eq!(openai["id"], json!("openai"));
    assert_eq!(openai["label"], json!("OpenAI"));
    assert_eq!(openai["kind"], json!("openai"));
    assert_eq!(openai["baseUrl"], json!("https://api.example/v1"));
    assert_eq!(openai["apiKey"], json!("sk-test"));
    assert_eq!(openai["defaultModel"], json!("gpt-5"));
    assert_eq!(openai["enabled"], json!(true));
    assert_eq!(openai["extraHeaders"]["X-Trace"], json!("on"));
    assert_eq!(openai["metadata"]["tier"], json!("pro"));
    assert_eq!(openai.as_object().unwrap().len(), 9);
    assert_eq!(
        store.get_provider("openai").await.unwrap().unwrap()["label"],
        json!("OpenAI")
    );

    let models = store.list_provider_models("openai").await.unwrap();
    assert_eq!(
        models
            .iter()
            .map(|model| model["modelId"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["gpt-5", "gpt-mini"]
    );
    assert_eq!(models[0]["providerId"], json!("openai"));
    assert_eq!(models[0]["displayName"], json!("GPT-5"));
    assert_eq!(models[0]["contextWindowTokens"].as_i64(), Some(128_000));
    assert_eq!(models[0]["supportsToolCalling"], json!(true));
    assert_eq!(models[0]["fetchedAt"], json!(T2));
    assert_eq!(models[1]["displayName"], Value::Null);
    assert!(models[1].get("contextWindowTokens").is_none());
    assert!(models[1].get("supportsToolCalling").is_none());

    let capabilities = store
        .get_provider_capabilities("openai")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            capabilities.get("streaming"),
            capabilities.get("toolCalling"),
            capabilities.get("modelList"),
            capabilities.get("vision")
        ),
        (
            Some(&json!(true)),
            Some(&json!(true)),
            Some(&json!(true)),
            Some(&json!(true))
        )
    );
    assert!(capabilities.get("jsonMode").is_none());
    assert_eq!(capabilities["maxContextTokens"].as_i64(), Some(200_000));
    assert_eq!(capabilities["checkedAt"], json!(T3));
    assert_eq!(capabilities["warning"], json!("rate limited"));

    let server = store
        .get_setting(MCP_SERVERS_NAMESPACE, "server-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(server["id"], json!("server-1"));
    assert_eq!(server["alias"], json!("fs"));
    assert_eq!(server["transport"]["kind"], json!("stdio"));
    assert_eq!(server["transport"]["command"], json!("npx"));
    assert_eq!(server["secretRefs"]["TOKEN"], json!("secret://x"));
    assert_eq!(server["toolAliases"]["ls"], json!("list"));
    // A NULL column is an ABSENT `enabled`, not a false one.
    assert!(server.get("enabled").is_none());
    assert!(server.get("resilience").is_none());
    assert_eq!(
        (server["createdAt"].as_str(), server["updatedAt"].as_str()),
        (Some(T0), Some(T1))
    );

    let settings = store
        .get_setting(ASSISTANT_SETTINGS_NAMESPACE, ASSISTANT_SETTINGS_KEY)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(settings["defaultProviderId"], json!("openai"));
    assert_eq!(settings["defaultModel"], json!("gpt-5"));
    assert_eq!(settings["contextSizePreference"], json!("small"));
    assert_eq!(
        settings["writePolicyMode"],
        json!("auto_readonly_confirm_writes")
    );
    assert_eq!(settings["allowRawToolData"], json!(false));
    assert_eq!(settings["maxToolIterations"].as_i64(), Some(24));
    assert_eq!(settings["toolCalling"], json!("auto"));
    assert_eq!(settings["metadata"]["theme"], json!("dark"));

    let scope = store.get_run_scope("run-1").await.unwrap().unwrap();
    assert_eq!(scope["protocol"], json!("responses"));
    assert_eq!(scope["chatId"], json!("chat-1"));
    assert_eq!(scope["generation"].as_i64(), Some(1));
    let continuation = store.get_continuation("run-1").await.unwrap().unwrap();
    assert_eq!(continuation.run_id, "run-1");
    assert_eq!(continuation.anchor_message_id, "m2");
    assert_eq!(continuation.state["messageCount"].as_i64(), Some(3));
    assert_eq!(continuation.state["scope"]["model"], json!("gpt-5"));
    assert_eq!(
        store
            .continuations_by_anchors("chat-1", vec!["m2".into()])
            .await
            .unwrap()
            .len(),
        1
    );

    // Nothing is dropped in silence: a table the Rust store cannot represent is
    // named, as is the derived index the import deliberately does not copy.
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.starts_with("message_search:")),
        "{:?}",
        report.warnings
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.starts_with("receipts:")),
    );
}

#[tokio::test]
async fn importing_twice_is_idempotent_and_the_file_reopens_as_a_rust_store() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("ts.db");
    let destination = root.path().join("agentkit.db");
    ts_database(&source);
    let untouched = std::fs::read(&source).unwrap();

    let store = SqliteStore::open(&destination, 8).unwrap();
    let first = store
        .import_from_ts_schema(&source, &options())
        .await
        .unwrap();
    let second = store
        .import_from_ts_schema(&source, &options())
        .await
        .unwrap();
    assert_eq!(
        (
            first.chats,
            first.messages,
            first.tasks,
            first.events,
            first.attempts,
            first.proposals,
            first.receipts,
            first.outbox,
            first.providers,
            first.mcp_servers
        ),
        (
            second.chats,
            second.messages,
            second.tasks,
            second.events,
            second.attempts,
            second.proposals,
            second.receipts,
            second.outbox,
            second.providers,
            second.mcp_servers
        ),
        "{first:?} vs {second:?}"
    );
    // The second pass refreshed in place; it duplicated nothing.
    assert_eq!(store.list_chats(true).await.unwrap().len(), 2);
    assert_eq!(store.messages("chat-1", true).await.unwrap().len(), 6);
    assert_eq!(store.list_tasks(None).await.unwrap().len(), 3);
    assert_eq!(store.attempts("run-1").await.unwrap().len(), 2);
    assert_eq!(store.events("run-1", None, 10).await.unwrap().len(), 2);
    assert_eq!(store.outbox().await.unwrap().len(), 1);
    assert_eq!(store.list_providers().await.unwrap().len(), 1);
    assert_eq!(store.list_provider_models("openai").await.unwrap().len(), 2);
    assert!(store.receipt("op-1").await.unwrap().is_some());
    assert_eq!(
        store
            .get_lease("run-1")
            .await
            .unwrap()
            .unwrap()
            .fencing_token,
        7
    );
    // The source was read, never written.
    assert_eq!(std::fs::read(&source).unwrap(), untouched);
    drop(store);

    let reopened = SqliteStore::open(&destination, 8).expect("an imported file is a Rust store");
    assert_eq!(
        reopened
            .get_chat("chat-1")
            .await
            .unwrap()
            .unwrap()
            .title
            .as_deref(),
        Some("Working")
    );
    assert_eq!(reopened.list_tasks(None).await.unwrap().len(), 3);
    assert_eq!(
        reopened.get_task("run-1").await.unwrap().unwrap().status,
        TaskStatus::Running
    );
    drop(reopened);

    let raw = Connection::open(&destination).unwrap();
    let identity: i64 = raw
        .query_row("PRAGMA application_id", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        identity, 0x414b_5253,
        "the AKRS identity survived the import"
    );
    let version: i64 = raw
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    let mut check = raw.prepare("PRAGMA foreign_key_check").unwrap();
    let mut violations = check.query([]).unwrap();
    assert!(
        violations.next().unwrap().is_none(),
        "the import left no dangling key"
    );
}

#[tokio::test]
async fn anything_that_is_not_a_typescript_assistant_store_is_refused_not_panicking() {
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("agentkit.db");
    let store = SqliteStore::open(&destination, 8).unwrap();

    let garbage = root.path().join("garbage.db");
    std::fs::write(&garbage, b"definitely not the SQLite header you expect").unwrap();
    let error = store
        .import_from_ts_schema(&garbage, &options())
        .await
        .unwrap_err();
    assert!(
        matches!(error, StoreError::InvalidImport { .. }),
        "garbage is refused: {error}"
    );

    let missing = root.path().join("nowhere.db");
    assert!(
        store
            .import_from_ts_schema(&missing, &options())
            .await
            .unwrap_err()
            .to_string()
            .contains("nowhere.db")
    );

    // A real SQLite file that is simply not an assistant store.
    let unrelated = root.path().join("notes.db");
    let raw = Connection::open(&unrelated).unwrap();
    raw.execute_batch("CREATE TABLE notes(id TEXT PRIMARY KEY, body TEXT NOT NULL);")
        .unwrap();
    raw.pragma_update(None, "user_version", 10).unwrap();
    drop(raw);
    assert!(matches!(
        store
            .import_from_ts_schema(&unrelated, &options())
            .await
            .unwrap_err(),
        StoreError::InvalidImport { .. }
    ));

    // A database at a schema version this importer never saw.
    let future = root.path().join("future.db");
    let raw = Connection::open(&future).unwrap();
    raw.execute_batch("CREATE TABLE chats(id TEXT PRIMARY KEY);")
        .unwrap();
    raw.pragma_update(None, "user_version", 99).unwrap();
    drop(raw);
    assert!(
        store
            .import_from_ts_schema(&future, &options())
            .await
            .unwrap_err()
            .to_string()
            .contains("99")
    );

    // A Rust store is a foreign database, not a source.
    let rust = root.path().join("rust.db");
    drop(SqliteStore::open(&rust, 8).unwrap());
    assert!(matches!(
        store
            .import_from_ts_schema(&rust, &options())
            .await
            .unwrap_err(),
        StoreError::ForeignDatabase
    ));

    // And a store never imports itself.
    assert!(
        store
            .import_from_ts_schema(&destination, &options())
            .await
            .unwrap_err()
            .to_string()
            .contains("own file")
    );

    // None of that touched the destination.
    assert!(store.list_chats(true).await.unwrap().is_empty());
    assert!(store.list_tasks(None).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_dry_run_counts_without_writing_and_a_tolerant_import_keeps_the_rest() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("ts.db");
    let destination = root.path().join("agentkit.db");
    ts_database(&source);
    let store = SqliteStore::open(&destination, 8).unwrap();

    let report = store
        .import_from_ts_schema(
            &source,
            &ImportOptions {
                dry_run: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        report.chats, 2,
        "a dry run still counts what it would write"
    );
    assert_eq!(report.messages, 7);
    assert!(store.get_chat("chat-1").await.unwrap().is_none());
    assert!(store.list_tasks(None).await.unwrap().is_empty());
    assert!(store.outbox().await.unwrap().is_empty());

    // The same file then commits, with the same counts.
    let report = store
        .import_from_ts_schema(&source, &options())
        .await
        .unwrap();
    assert_eq!(report.chats, 2);
    assert!(store.get_chat("chat-1").await.unwrap().is_some());

    // A file with rows that cannot be placed.
    let broken = root.path().join("broken.db");
    ts_database(&broken);
    let raw = Connection::open(&broken).unwrap();
    raw.execute_batch(
        "PRAGMA foreign_keys=OFF;
INSERT INTO messages (id,chat_id,run_id,role,content,content_format,order_key,parent_message_id,
    depth,branch_index,active,metadata,created_at)
  VALUES ('orphan','chat-1',NULL,'user','dangling','text',9,'ghost',3,0,1,'{}','2026-06-06T00:00:00.000Z');
INSERT INTO tasks (task_id,kind,scope_id,status,priority,enqueued_at,available_at,payload,
    attempt_count,poison_count)
  VALUES ('bad-1','k','s','exploded',0,'2026-06-06T00:00:00.000Z','2026-06-06T00:00:00.000Z','{}',0,0);
INSERT INTO proposal_outcomes VALUES ('op-404','applied',0,'[]',NULL,NULL);
INSERT INTO task_events VALUES ('run-1',2,'event-3',NULL,'run.rebuilt','2026-06-06T00:00:00.000Z','\"plain\"');",
    )
    .unwrap();
    drop(raw);

    let error = store
        .import_from_ts_schema(
            &broken,
            &ImportOptions {
                abort_on_error: true,
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::InvalidImport { .. }), "{error}");

    let report = store
        .import_from_ts_schema(
            &broken,
            &ImportOptions {
                abort_on_error: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(report.chats, 2, "the healthy rows are still imported");
    assert_eq!(
        report.messages, 7,
        "the orphan is refused, not placed anywhere"
    );
    assert_eq!(report.tasks, 3, "the unknown task status is refused");
    assert_eq!(report.receipts, 1, "an outcome with no proposal is refused");
    assert_eq!(
        report.events, 3,
        "a non-object payload is rebuilt from the columns"
    );
    for needle in ["orphan", "bad-1", "op-404", "event-3"] {
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains(needle)),
            "{needle} is reported: {:?}",
            report.warnings
        );
    }
    let rebuilt = store.events("run-1", Some(1), 10).await.unwrap();
    assert_eq!(rebuilt[0].event["eventId"], json!("event-3"));
    assert_eq!(rebuilt[0].event["type"], json!("run.rebuilt"));
    assert_eq!(rebuilt[0].seq, 2.0);
    assert!(store.get_task("run-1").await.unwrap().is_some());
}

#[tokio::test]
async fn session_inputs_and_context_summaries_round_trip_as_typed_records() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("ts.db");
    let destination = root.path().join("agentkit.db");
    ts_database(&source);
    let store = SqliteStore::open(&destination, 8).unwrap();
    let report = store
        .import_from_ts_schema(&source, &options())
        .await
        .unwrap();
    assert_eq!(
        (report.agent_inputs, report.context_summaries),
        (2, 1),
        "{report:?}"
    );

    let inputs = store
        .list_inputs(AgentInputQuery {
            session_id: "chat-1".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(inputs.len(), 1);
    let input = &inputs[0];
    assert_eq!(
        (input.input_id.as_str(), input.session_id.as_str()),
        ("input-1", "chat-1")
    );
    assert_eq!(
        (input.kind, input.status),
        (AgentInputKind::Steer, AgentInputStatus::Pending)
    );
    assert_eq!(input.content, json!({"text": "go"}));
    assert_eq!(input.content_hash, "hash");
    assert_eq!(input.order, 3);
    assert_eq!(input.target_task_id.as_deref(), Some("run-1"));
    assert_eq!(input.on_settled, Some(AgentInputSettlePolicy::FollowUp));
    assert_eq!(
        (input.created_at.as_str(), input.updated_at.as_str()),
        (T0, T0)
    );

    let answered = store
        .get_input("input-2".to_owned())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (answered.kind, answered.status),
        (AgentInputKind::HumanRequest, AgentInputStatus::Answered)
    );
    assert_eq!(
        (
            answered.attempt_id.as_deref(),
            answered.answer_id.as_deref(),
            answered.expires_at.as_deref()
        ),
        (Some("a2"), Some("answer-1"), Some(T3))
    );
    assert_eq!(answered.on_settled, None);

    // The FIFO counter sits above every imported order, so an input created
    // after the import can never take an imported position.
    let (fresh, created) = store
        .create_input(
            CreateAgentInput {
                input_id: "input-9".into(),
                session_id: "chat-1".into(),
                kind: AgentInputKind::Steer,
                content: json!("later"),
                content_hash: "h".into(),
                target_task_id: None,
                on_settled: None,
                attempt_id: None,
                tool_call_id: None,
                expires_at: None,
            },
            T4_MS,
        )
        .await
        .unwrap();
    assert!(created);
    assert_eq!(fresh.order, 8);

    let summaries = store.list_context_summaries("chat-1").await.unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0]["summaryId"], json!("sum-1"));
    assert_eq!(summaries[0]["coversThroughMessageId"], json!("m2"));
    assert_eq!(summaries[0]["tokensBefore"].as_i64(), Some(1200));
    assert_eq!(summaries[0]["tokensAfter"].as_i64(), Some(300));
    assert_eq!(summaries[0]["providerId"], json!("openai"));
    assert_eq!(summaries[0]["details"]["readFiles"], json!(["a.txt"]));
    assert_eq!(summaries[0]["createdAt"], json!(T2));

    // A second pass refreshes the same rows instead of adding copies.
    let again = store
        .import_from_ts_schema(&source, &options())
        .await
        .unwrap();
    assert_eq!(
        (again.agent_inputs, again.context_summaries),
        (2, 1),
        "{again:?}"
    );
    assert_eq!(
        store
            .list_inputs(AgentInputQuery {
                session_id: "chat-1".into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        // The imported input plus the one this test created — no duplicates.
        ["input-1", "input-9"]
    );
    assert_eq!(
        store.list_context_summaries("chat-1").await.unwrap().len(),
        1
    );
}

/// The optional `tool_executions` table, column for column from the
/// TypeScript adapter's `TOOL_EXECUTIONS_TABLE_SQL`.
const TOOL_EXECUTIONS_TABLE: &str = "CREATE TABLE tool_executions (\
  execution_id TEXT PRIMARY KEY, \
  run_id TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE, \
  approval_request_id TEXT UNIQUE, record TEXT NOT NULL)";

fn sha_hex(digit: char) -> String {
    digit.to_string().repeat(64)
}

/// A TypeScript `ToolExecutionRecord` exactly as `tool-execution-store.ts`
/// would have written it — note there is NO `chatId` field.
fn execution_record(
    id: &str,
    run: &str,
    call: &str,
    tool: &str,
    status: &str,
    result: bool,
) -> String {
    let mut record = json!({
        "executionId": id,
        "runId": run,
        "toolCallId": call,
        "toolName": tool,
        "argsSha256": sha_hex('a'),
        "contextSha256": sha_hex('b'),
        "attemptId": "attempt-1",
        "status": status,
    });
    if result {
        // The REAL `ToolResult` wire shape — this is what the host's replay
        // path deserializes, so the fixture must be a value production
        // could have written, not a `{content,isError}` stand-in.
        record["result"] = json!({
            "ok": true,
            "data": {"content": "ok"},
            "sources": [],
            "warnings": [],
            "truncated": false,
            "limits": {"profile": "small", "maxBytes": 16384, "maxItems": 50},
        });
    }
    record.to_string()
}

fn seed_executions(path: &Path, rows: &[(&str, &str, &str, &str, &str, bool)]) {
    let db = Connection::open(path).unwrap();
    db.execute_batch(TOOL_EXECUTIONS_TABLE).unwrap();
    for (id, run, call, tool, status, result) in rows {
        db.execute(
            "INSERT INTO tool_executions(execution_id,run_id,record) VALUES(?,?,?)",
            rusqlite::params![
                id,
                run,
                execution_record(id, run, call, tool, status, *result)
            ],
        )
        .unwrap();
    }
    drop(db);
}

#[tokio::test]
async fn uncertain_and_completed_executions_survive_the_import_as_barriers() {
    // The receipts that keep a crash-uncertain mutation from running twice
    // must outlive the upgrade: an admitted-without-result effect stays
    // blocked, and a completed one keeps its result for replay — including
    // the chat the TypeScript record does not store, which the import must
    // re-derive from the run's binding.
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("ts.db");
    ts_database(&source);
    seed_executions(
        &source,
        &[
            (
                "exec-uncertain",
                "run-1",
                "call-x",
                "write_file",
                "admitted",
                false,
            ),
            (
                "exec-done",
                "run-1",
                "call-1",
                "read_file",
                "completed",
                true,
            ),
        ],
    );
    let destination = root.path().join("agentkit.db");
    let store = SqliteStore::open(&destination, 8).unwrap();
    let report = store
        .import_from_ts_schema(&source, &options())
        .await
        .unwrap();
    assert_eq!(report.executions, 2, "{report:?}");

    let run = store.list_executions("run-1".to_string()).await.unwrap();
    assert_eq!(run.len(), 2);
    assert_eq!(run[0].execution_id, "exec-uncertain");
    assert_eq!(run[0].status, ExecutionReceiptStatus::Admitted);
    assert_eq!(
        run[0].chat_id, "chat-1",
        "the chat is re-derived from the run binding"
    );
    assert_eq!(
        run[0].result, None,
        "uncertainty is admitted-without-result"
    );
    assert_eq!(run[1].status, ExecutionReceiptStatus::Completed);
    assert_eq!(
        run[1].result,
        Some(json!({
            "ok": true,
            "data": {"content": "ok"},
            "sources": [],
            "warnings": [],
            "truncated": false,
            "limits": {"profile": "small", "maxBytes": 16384, "maxItems": 50},
        })),
        "the stored result is the ToolResult shape the host replays"
    );

    // The cross-run same-chat guard reads exactly this set.
    let chat = store
        .list_executions_by_chat("chat-1".to_string())
        .await
        .unwrap();
    assert_eq!(chat.len(), 2);
    assert!(
        chat.iter()
            .any(|receipt| receipt.execution_id == "exec-uncertain")
    );

    // Re-running the import refreshes rather than duplicates.
    let again = store
        .import_from_ts_schema(&source, &options())
        .await
        .unwrap();
    assert_eq!(again.executions, 2);
    assert_eq!(
        store
            .list_executions("run-1".to_string())
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn an_execution_whose_chat_cannot_be_proven_is_quarantined() {
    // `dep-1` has no provider binding and no messages carrying the run, so
    // the chat-scoped barrier cannot be rebuilt for its receipt: strict mode
    // refuses the whole import; tolerant mode refuses the row loudly while
    // importing the provable ones.
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("ts.db");
    ts_database(&source);
    seed_executions(
        &source,
        &[
            (
                "exec-uncertain",
                "run-1",
                "call-x",
                "write_file",
                "admitted",
                false,
            ),
            (
                "exec-orphan",
                "dep-1",
                "call-y",
                "delete_dir",
                "admitted",
                false,
            ),
        ],
    );
    let destination = root.path().join("agentkit.db");
    let store = SqliteStore::open(&destination, 8).unwrap();
    let error = store
        .import_from_ts_schema(&source, &options())
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::InvalidImport { .. }), "{error}");
    assert!(
        store.list_chats(false).await.unwrap().is_empty(),
        "the refused import leaves the destination untouched"
    );

    let report = store
        .import_from_ts_schema(
            &source,
            &ImportOptions {
                abort_on_error: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        report.executions, 1,
        "only the provable receipt is imported"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("exec-orphan")
                && warning.contains("binds to no single chat")),
        "{:?}",
        report.warnings
    );
    assert!(
        store
            .list_executions("dep-1".to_string())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .list_executions("run-1".to_string())
            .await
            .unwrap()
            .len(),
        1
    );
    // The unplaced uncertain receipt is NOT silently omitted: tolerant mode
    // records a durable uncertainty barrier keyed by the best-known
    // identity, so the identical effect stays blocked until a human opens
    // this exact barrier id.
    assert_eq!(
        report.uncertainty_barriers, 1,
        "warning plus omission would delete the evidence of a possibly-executed effect"
    );
    let barriers = store
        .list_uncertainty_barriers("chat-1".to_string(), "dep-1".to_string())
        .await
        .unwrap();
    assert_eq!(barriers.len(), 1, "{barriers:?}");
    let barrier = &barriers[0];
    assert_eq!(barrier.tool_name, "delete_dir");
    assert_eq!(
        barrier.args_hash_provenance,
        agentkit_store::ArgsHashProvenance::TsCanonical
    );
    assert_eq!(barrier.run_id.as_deref(), Some("dep-1"));
    assert!(
        barrier
            .args_sha256
            .as_deref()
            .is_some_and(|hash| hash.len() == 64),
        "the best-known argument fingerprint survives: {:?}",
        barrier.args_sha256
    );
    assert!(barrier.reason.contains("binds to no single chat"));
    // The barrier speaks the compatibility language: an invocation whose
    // TS-pipeline digest matches is blocked by name+identity.
    assert!(barrier.blocks(
        "delete_dir",
        &"0".repeat(64),
        barrier.args_sha256.as_deref(),
    ));
    assert!(!barrier.blocks("write_file", &"0".repeat(64), None));
}

#[tokio::test]
async fn an_execution_column_or_record_mismatch_never_reaches_the_guard() {
    // A receipt the destination's anti-corruption validator would reject on
    // read (here: the record's executionId disagrees with the column) must be
    // refused at import, because such a row would fail-closed every later
    // recovery read.
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("ts.db");
    ts_database(&source);
    let db = Connection::open(&source).unwrap();
    db.execute_batch(TOOL_EXECUTIONS_TABLE).unwrap();
    db.execute(
        "INSERT INTO tool_executions(execution_id,run_id,record) VALUES(?,?,?)",
        rusqlite::params![
            "exec-1",
            "run-1",
            execution_record(
                "exec-OTHER",
                "run-1",
                "call-1",
                "write_file",
                "admitted",
                false
            )
        ],
    )
    .unwrap();
    // An unknown status vocabulary is refused as well.
    db.execute(
        "INSERT INTO tool_executions(execution_id,run_id,record) VALUES(?,?,?)",
        rusqlite::params![
            "exec-2",
            "run-1",
            execution_record("exec-2", "run-1", "call-2", "write_file", "running", false)
        ],
    )
    .unwrap();
    drop(db);

    let destination = root.path().join("agentkit.db");
    let store = SqliteStore::open(&destination, 8).unwrap();
    let error = store
        .import_from_ts_schema(&source, &options())
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::InvalidImport { .. }), "{error}");
    assert!(
        store
            .list_executions("run-1".to_string())
            .await
            .unwrap()
            .is_empty()
    );

    let report = store
        .import_from_ts_schema(
            &source,
            &ImportOptions {
                abort_on_error: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(report.executions, 0);
    assert!(report.warnings.iter().any(|w| w.contains("exec-1")));
    assert!(report.warnings.iter().any(|w| w.contains("exec-2")));
    assert!(
        store
            .list_executions("run-1".to_string())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn strict_import_refuses_an_orphan_only_database_and_leaves_it_untouched() {
    // The R03 regression: a lone orphan used to satisfy `skip` and then have
    // its error converted to success, committing an import with the orphan
    // silently missing. Strict mode must refuse, and the transaction must
    // roll every sibling row back with it.
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("ts.db");
    ts_database(&source);
    let raw = Connection::open(&source).unwrap();
    raw.execute_batch(
        "PRAGMA foreign_keys=OFF;
INSERT INTO messages (id,chat_id,run_id,role,content,content_format,order_key,parent_message_id,
    depth,branch_index,active,metadata,created_at)
  VALUES ('orphan','chat-1',NULL,'user','dangling','text',9,'ghost',3,0,1,'{}','2026-06-06T00:00:00.000Z');",
    )
    .unwrap();
    drop(raw);

    let destination = root.path().join("agentkit.db");
    let store = SqliteStore::open(&destination, 8).unwrap();
    let error = store
        .import_from_ts_schema(&source, &options())
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::InvalidImport { .. }), "{error}");
    assert!(
        error.to_string().contains("orphan"),
        "the refusal names the orphan: {error}"
    );
    assert!(
        store.list_chats(false).await.unwrap().is_empty(),
        "a strict refusal leaves no partially imported history behind"
    );
    assert!(store.list_tasks(None).await.unwrap().is_empty());
}

#[tokio::test]
async fn strict_import_refuses_a_cycle_only_database_and_tolerant_names_it() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("ts.db");
    ts_database(&source);
    let raw = Connection::open(&source).unwrap();
    raw.execute_batch(
        "PRAGMA foreign_keys=OFF;
INSERT INTO messages (id,chat_id,run_id,role,content,content_format,order_key,parent_message_id,
    depth,branch_index,active,metadata,created_at)
  VALUES ('loop-a','chat-1',NULL,'user','a','text',7,'loop-b',3,0,1,'{}','2026-06-06T00:00:00.000Z'),
         ('loop-b','chat-1',NULL,'assistant','b','text',8,'loop-a',4,0,1,'{}','2026-06-06T00:00:00.000Z');",
    )
    .unwrap();
    drop(raw);

    let destination = root.path().join("agentkit.db");
    let store = SqliteStore::open(&destination, 8).unwrap();
    let error = store
        .import_from_ts_schema(&source, &options())
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::InvalidImport { .. }), "{error}");
    assert!(
        store.list_chats(false).await.unwrap().is_empty(),
        "the cycle refuses the whole import"
    );

    let report = store
        .import_from_ts_schema(
            &source,
            &ImportOptions {
                abort_on_error: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(report.messages, 7, "the placed messages, and only those");
    for member in ["loop-a", "loop-b"] {
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains(member)),
            "{member} is reported: {:?}",
            report.warnings
        );
    }
}

#[tokio::test]
async fn imported_settings_reach_the_production_settings_document() {
    // The R05 regression: `toolCalling: "off"` written only to the
    // compatibility namespace never reached `ChatExecutor::stage_tools`,
    // which reads ("http","settings"). The promotion must carry the whole
    // whitelist and must never overwrite a live document.
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("ts.db");
    ts_database(&source);
    let raw = Connection::open(&source).unwrap();
    raw.execute("UPDATE settings SET tool_calling_mode='off' WHERE id=1", [])
        .unwrap();
    drop(raw);

    let destination = root.path().join("agentkit.db");
    let store = SqliteStore::open(&destination, 8).unwrap();
    let report = store
        .import_from_ts_schema(&source, &options())
        .await
        .unwrap();
    assert_eq!(report.promoted_settings, 1);
    assert_eq!(
        store
            .get_setting(SETTINGS_NAMESPACE, SETTINGS_KEY)
            .await
            .unwrap(),
        Some(json!({
            "defaultProviderId": "openai",
            "defaultModel": "gpt-5",
            "contextSizePreference": "small",
            "writePolicyMode": "auto_readonly_confirm_writes",
            "allowRawToolData": false,
            "maxToolIterations": 24,
            "toolCalling": "off",
            "metadata": {"theme": "dark"},
        })),
        "the executor's policy gate reads exactly this document"
    );
    // The compatibility copy is still there for auditing.
    assert!(
        store
            .get_setting(ASSISTANT_SETTINGS_NAMESPACE, ASSISTANT_SETTINGS_KEY)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn promotion_never_overwrites_a_live_settings_document() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("ts.db");
    ts_database(&source);
    let destination = root.path().join("agentkit.db");
    let store = SqliteStore::open(&destination, 8).unwrap();
    let live = json!({"toolCalling": "on", "contextSizePreference": "large"});
    store
        .set_setting(SETTINGS_NAMESPACE, SETTINGS_KEY, live.clone())
        .await
        .unwrap();

    let report = store
        .import_from_ts_schema(&source, &options())
        .await
        .unwrap();
    assert_eq!(report.promoted_settings, 0);
    assert_eq!(report.preserved_settings, 1);
    assert_eq!(
        store
            .get_setting(SETTINGS_NAMESPACE, SETTINGS_KEY)
            .await
            .unwrap(),
        Some(live)
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("already exists")),
        "{:?}",
        report.warnings
    );
}

#[tokio::test]
async fn imported_mcp_servers_aggregate_into_the_production_config_document() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("ts.db");
    ts_database(&source);
    let raw = Connection::open(&source).unwrap();
    // A second server whose alias the production validator could never accept.
    raw.execute(
        "INSERT INTO mcp_servers VALUES ('server-2','Bad Alias','{\"kind\":\"http\",\"url\":\"https://m.example\"}',NULL,1,NULL,NULL,'2026-06-06T00:00:00.000Z','2026-06-06T00:00:00.000Z')",
        [],
    )
    .unwrap();
    drop(raw);

    let destination = root.path().join("agentkit.db");
    let store = SqliteStore::open(&destination, 8).unwrap();
    let report = store
        .import_from_ts_schema(
            &source,
            &ImportOptions {
                promote_mcp_application_id: Some("onemind".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(report.mcp_servers, 2, "both servers stay in the archive");
    assert_eq!(report.promoted_mcp_servers, 1);
    assert_eq!(
        store
            .get_setting(MCP_CONFIG_NAMESPACE, "onemind")
            .await
            .unwrap(),
        Some(json!([{
            "id": "server-1",
            "alias": "fs",
            "transport": {"kind": "stdio", "command": "npx"},
            "secretRefs": {"TOKEN": "secret://x"},
            "toolAliases": {"ls": "list"},
            "createdAt": T0,
            "updatedAt": T1,
        }])),
        "the array document McpConfigRepository::read deserializes"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("server-2") && warning.contains("alias")),
        "the excluded server is reported: {:?}",
        report.warnings
    );
    // Without an application id, production is left alone entirely.
    let other = SqliteStore::open(root.path().join("other.db"), 8).unwrap();
    other
        .import_from_ts_schema(&source, &options())
        .await
        .unwrap();
    assert!(
        other
            .get_setting(MCP_CONFIG_NAMESPACE, "onemind")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn mcp_promotion_never_overwrites_a_live_config_document() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("ts.db");
    ts_database(&source);
    let destination = root.path().join("agentkit.db");
    let store = SqliteStore::open(&destination, 8).unwrap();
    let live = json!([{"id": "live-1", "alias": "live", "transport": {"kind": "http"}}]);
    store
        .set_setting(MCP_CONFIG_NAMESPACE, "onemind", live.clone())
        .await
        .unwrap();
    let report = store
        .import_from_ts_schema(
            &source,
            &ImportOptions {
                promote_mcp_application_id: Some("onemind".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(report.promoted_mcp_servers, 0);
    assert_eq!(
        store
            .get_setting(MCP_CONFIG_NAMESPACE, "onemind")
            .await
            .unwrap(),
        Some(live)
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("live configuration document")),
        "{:?}",
        report.warnings
    );
    assert_eq!(report.preserved_mcp_configs, 1);
}

/// `McpServerDtoSchema.resilience` is `Record<string, unknown>` — numeric
/// `requestTimeoutMs` and boolean flags are contract-legal, so promotion
/// must carry them untouched; and `McpTransportDtoSchema` is a CLOSED
/// union (stdio requires `command`, http requires `url`), so malformed
/// transports are refused AT PROMOTION with a warning and a conflict
/// count instead of poisoning the live document for the reader.
#[tokio::test]
async fn resilience_values_promote_and_malformed_transports_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("ts.db");
    ts_database(&source);
    let raw = Connection::open(&source).unwrap();
    for (id, alias, transport, resilience) in [
        (
            "server-3",
            "net",
            "{\"kind\":\"http\",\"url\":\"https://m.example\"}",
            Some(
                "{\"requestTimeoutMs\":2500,\"retryOnDisconnect\":true,\"backoff\":{\"base\":100}}",
            ),
        ),
        ("server-4", "broken", "{\"kind\":\"http\"}", None),
        ("server-5", "nostd", "{\"kind\":\"stdio\"}", None),
        (
            "server-6",
            "weird",
            "{\"kind\":\"grpc\",\"url\":\"https://m.example\"}",
            None,
        ),
        (
            "server-7",
            "badres",
            "{\"kind\":\"stdio\",\"command\":\"npx\"}",
            Some("[1,2]"),
        ),
    ] {
        raw.execute(
            "INSERT INTO mcp_servers VALUES (?1,?2,?3,NULL,1,NULL,?4,'2026-06-06T00:00:00.000Z','2026-06-06T00:00:00.000Z')",
            rusqlite::params![id, alias, transport, resilience],
        )
        .unwrap();
    }
    drop(raw);

    let destination = root.path().join("agentkit.db");
    let store = SqliteStore::open(&destination, 8).unwrap();
    let report = store
        .import_from_ts_schema(
            &source,
            &ImportOptions {
                promote_mcp_application_id: Some("onemind".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let document = store
        .get_setting(MCP_CONFIG_NAMESPACE, "onemind")
        .await
        .unwrap()
        .expect("the live document exists");
    let aliases: Vec<&str> = document
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["alias"].as_str().unwrap())
        .collect();
    assert_eq!(report.mcp_servers, 6, "the archive keeps every server");
    // server-7's array `resilience` is dropped AT SHREDDING with a warning
    // (an optional field the column reader cannot shape); the transport
    // refusals below are what the promotion validator itself catches.
    assert_eq!(
        report.promoted_mcp_servers, 3,
        "server-1, server-3, and server-7 (minus its malformed field); carried: {aliases:?}"
    );
    assert_eq!(
        report.conflicted_mcp_servers, 3,
        "no-url http, no-command stdio, and the unknown transport kind"
    );
    let server = document
        .as_array()
        .expect("array document")
        .iter()
        .find(|entry| entry["alias"] == json!("net"))
        .expect("the numeric-resilience server promoted");
    assert_eq!(
        server["resilience"],
        json!({"requestTimeoutMs": 2500, "retryOnDisconnect": true, "backoff": {"base": 100}}),
        "arbitrary JSON values survive promotion exactly as the contract allows"
    );
    for (id, why) in [
        ("server-4", "closed DTO union"),
        ("server-5", "closed DTO union"),
        ("server-6", "closed DTO union"),
        ("server-7", "resilience"),
    ] {
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains(id) && warning.contains(why)),
            "{id} was not refused with a {why}-shaped warning: {:?}",
            report.warnings
        );
    }
    // The reader's own gate accepts the promoted document; none of the
    // transport-refused servers leaked into it.
    assert_eq!(aliases, ["fs", "net", "badres"]);
    let badres = document
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["alias"] == json!("badres"))
        .expect("server-7 promoted without its malformed field");
    assert!(
        badres.get("resilience").is_none(),
        "the unshapable optional field was dropped, not smuggled: {badres}"
    );
}

/// The user deleted every MCP server: the live document is the EMPTY
/// array, which is still a live decision. A reimport must not resurrect
/// the servers — the same no-overwrite rule guards `[]` as any other
/// array.
#[tokio::test]
async fn deleting_all_servers_stays_deleted_through_a_reimport() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("ts.db");
    ts_database(&source);
    let destination = root.path().join("agentkit.db");
    let store = SqliteStore::open(&destination, 8).unwrap();
    store
        .set_setting(MCP_CONFIG_NAMESPACE, "onemind", json!([]))
        .await
        .unwrap();
    let report = store
        .import_from_ts_schema(
            &source,
            &ImportOptions {
                promote_mcp_application_id: Some("onemind".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(report.promoted_mcp_servers, 0);
    assert_eq!(report.preserved_mcp_configs, 1);
    assert_eq!(
        store
            .get_setting(MCP_CONFIG_NAMESPACE, "onemind")
            .await
            .unwrap(),
        Some(json!([])),
        "the delete-all decision is the user's, not the importer's"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("live configuration document")),
        "{:?}",
        report.warnings
    );
}

/// The durable search index is trigger-maintained state: the import's
/// direct `INSERT INTO messages ... ON CONFLICT` flows through the store's
/// INSERT trigger, so imported rows are searchable the moment they land —
/// plain text bodies and parts bodies alike. The inactive branch import
/// stays findable and the archived import stays hidden (query-time
/// visibility filter), and the source's own contentless `message_search`
/// is still reported as derived state that was deliberately not copied.
#[tokio::test]
async fn imported_messages_are_searchable_through_the_trigger_built_index() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("ts.db");
    let destination = root.path().join("agentkit.db");
    ts_database(&source);
    let store = SqliteStore::open(&destination, 8).unwrap();
    let report = store
        .import_from_ts_schema(&source, &options())
        .await
        .unwrap();
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.starts_with("message_search:")),
        "{:?}",
        report.warnings
    );
    let ids = |hits: Vec<MessageSearchHit>| -> Vec<String> {
        hits.into_iter().map(|hit| hit.message.id).collect()
    };
    assert_eq!(
        ids(store.search_messages("Hello", None, 10).await.unwrap()),
        ["m1"],
        "an imported text body is searchable"
    );
    assert_eq!(
        ids(store.search_messages("Hi", None, 10).await.unwrap()),
        ["m2"],
        "an imported parts body is searchable through the projection"
    );
    assert_eq!(
        ids(store.search_messages("Abandoned", None, 10).await.unwrap()),
        ["m7"],
        "an imported inactive branch stays searchable"
    );
    assert!(
        store
            .search_messages("Stale", None, 10)
            .await
            .unwrap()
            .is_empty(),
        "an imported archived message never surfaces"
    );
}

#[tokio::test]
async fn a_reimport_never_regresses_state_the_store_advanced() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("ts.db");
    ts_database(&source);
    let store = SqliteStore::open(root.path().join("agentkit.db"), 8).unwrap();
    store
        .import_from_ts_schema(&source, &options())
        .await
        .unwrap();
    let lease = store.get_lease("run-1").await.unwrap().unwrap();
    store
        .settle(SettleTask {
            fence: lease.fence(),
            now: T3_MS,
            status: TaskStatus::Completed,
            error: None,
            events: vec![],
            messages: vec![],
        })
        .await
        .unwrap();
    let again = store
        .import_from_ts_schema(&source, &options())
        .await
        .unwrap();
    let task = store.get_task("run-1").await.unwrap().unwrap();
    assert_eq!(
        task.status,
        TaskStatus::Completed,
        "the source said running"
    );
    assert!(
        store.get_lease("run-1").await.unwrap().is_none(),
        "a lease is never imported for a task that is not running"
    );
    assert!(
        again
            .warnings
            .iter()
            .any(|warning| warning.contains("task run-1: row already present; kept destination")),
        "{:?}",
        again.warnings
    );
}

#[tokio::test]
async fn a_child_listed_before_its_parent_still_imports_in_order() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("ts.db");
    ts_database(&source);
    let raw = Connection::open(&source).unwrap();
    raw.execute_batch(
        "PRAGMA foreign_keys=OFF;
INSERT INTO messages (id,chat_id,run_id,role,content,content_format,order_key,parent_message_id,
    depth,branch_index,active,metadata,created_at)
  VALUES ('grandchild','chat-1',NULL,'user','c','text',20,'child',2,0,0,'{}','2026-06-06T00:00:00.000Z'),
         ('child','chat-1',NULL,'assistant','b','text',21,'late-parent',1,0,0,'{}','2026-06-06T00:00:00.000Z'),
         ('late-parent','chat-1',NULL,'user','a','text',22,'m1',0,3,0,'{}','2026-06-06T00:00:00.000Z');",
    )
    .unwrap();
    drop(raw);
    let store = SqliteStore::open(root.path().join("agentkit.db"), 8).unwrap();
    let report = store
        .import_from_ts_schema(&source, &options())
        .await
        .unwrap();
    assert_eq!(report.messages, 10, "{:?}", report.warnings);
    let grandchild = store.get_message("grandchild").await.unwrap().unwrap();
    assert_eq!(grandchild.parent_message_id.as_deref(), Some("child"));
}

#[tokio::test]
async fn unrepresentable_numbers_and_instants_drop_the_row_with_a_warning() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("ts.db");
    ts_database(&source);
    let raw = Connection::open(&source).unwrap();
    raw.execute_batch(
        "INSERT INTO messages (id,chat_id,run_id,role,content,content_format,order_key,
    depth,branch_index,active,metadata,created_at)
  VALUES ('far-future','chat-1',NULL,'user','x','text',30,0,0,0,'{}','9000000000000000');
INSERT INTO agent_inputs VALUES ('input-big','chat-1','steer','pending','{}','h',
  9500000000000000,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,'2026-01-01T00:00:00.000Z','2026-01-01T00:00:00.000Z');
INSERT INTO agent_inputs VALUES ('input-frac','chat-1','steer','pending','{}','h',
  1.5,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,'2026-01-01T00:00:00.000Z','2026-01-01T00:00:00.000Z');",
    )
    .unwrap();
    drop(raw);
    let store = SqliteStore::open(root.path().join("agentkit.db"), 8).unwrap();
    // Strict mode refuses the out-of-range instant instead of erroring opaquely.
    let error = store
        .import_from_ts_schema(&source, &options())
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::InvalidImport { .. }), "{error}");
    let report = store
        .import_from_ts_schema(
            &source,
            &ImportOptions {
                abort_on_error: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(store.get_message("far-future").await.unwrap().is_none());
    for needle in ["far-future", "input-big", "input-frac"] {
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains(needle)),
            "{needle}: {:?}",
            report.warnings
        );
    }
    assert!(store.get_input("input-big".into()).await.unwrap().is_none());
}
