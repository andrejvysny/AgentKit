//! Dispatch-boundary fuzz (T18/T24 L1): hostile-but-parseable JSON, byte
//! corruption, cross-tenant splices and random value trees all go through
//! `WorkerService::handle`. Contract: always exactly one reply, always a
//! structured `Reply`, never a panic, never an authorization bypass -- and
//! the service is still fully functional afterwards.

mod support;

use agentkit_sandbox_contracts::PROTOCOL_VERSION;
use agentkit_sandbox_contracts::api::messages::{GuestFsOp, HostToWorker, WorkerToHost};
use agentkit_sandbox_contracts::error::SandboxErrorCode;
use agentkit_sandbox_contracts::grant::WriterLeaseToken;
use agentkit_sandbox_contracts::ids::{RequestDigest, TaskId};
use agentkit_sandbox_contracts::policy::SandboxCapability;
use agentkit_sandbox_contracts::protocol::{FrameHeader, WireFrame};
use agentkit_sandbox_worker::service::canonical_body;
use serde_json::Value;
use support::{Harness, gp, sid, tid};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
    fn value(&mut self, depth: u32) -> Value {
        use serde_json::Map;
        match self.below(if depth == 0 { 6 } else { 9 }) {
            0 => Value::Null,
            1 => Value::Bool(self.next() % 2 == 0),
            2 => Value::from(self.next() as i64),
            3 => Value::from(self.below(4_294_967_296) as u64),
            4 => Value::String(
                (0..self.below(30) + 1)
                    .map(|_| {
                        let pool = b"ab9_-./\\{}012";
                        pool[self.below(pool.len() as u64) as usize] as char
                    })
                    .collect(),
            ),
            5 => Value::from(self.below(1000) as i64),
            6 => Value::Array(
                (0..self.below(4) + 1)
                    .map(|_| self.value(depth - 1))
                    .collect(),
            ),
            7 => {
                let mut m = Map::new();
                for _ in 0..self.below(6) + 1 {
                    let key: String = (0..self.below(8) + 1)
                        .map(|_| {
                            let pool = b"abcdefgX012";
                            pool[self.below(pool.len() as u64) as usize] as char
                        })
                        .collect();
                    m.insert(key, self.value(depth - 1));
                }
                Value::Object(m)
            }
            _ => Value::Array(
                (0..self.below(3) + 1)
                    .map(|_| self.value(depth.saturating_sub(1)))
                    .collect(),
            ),
        }
    }
}

fn frame(request_id: &str, sequence: u64, body: Value) -> WireFrame {
    WireFrame {
        header: FrameHeader {
            protocol_version: PROTOCOL_VERSION,
            request_id: request_id.into(),
            sandbox_id: None,
            instance_generation: None,
            execution_id: None,
            operation: "fuzz".into(),
            sequence,
            deadline_ms: None,
            payload_len: 0,
            payload_digest: None,
        },
        body,
    }
}

async fn must_be_structured_reply(h: &Harness, seq: u64, body: Value) -> WorkerToHost {
    let reply = h
        .svc
        .handle(&frame(&format!("fuzz-{seq}"), seq, body))
        .await;
    // Hello/Ack paths are excluded by construction; everything else that
    // reaches `handle` answers exactly one Reply.
    assert!(
        matches!(reply, WorkerToHost::Reply { .. }),
        "fuzz frame produced {reply:?}"
    );
    reply
}

#[tokio::test(flavor = "multi_thread")]
async fn hostile_messages_reply_structurally_and_never_panic() {
    let h = Harness::new();
    h.create_ready("alice", "sbx").await;
    let mut rng = Rng(0xF00D_1234_5678_9ABC);

    // Valid, serializable templates for the splices below.
    let env = h.envelope(
        &tid("alice"),
        &sid("sbx"),
        support::genof(1),
        SandboxCapability::SandboxManage,
    );
    let task = TaskId::new("fuzz-task").unwrap();
    let lease: WriterLeaseToken = h
        .acquire_writer(&tid("alice"), &sid("sbx"), &task, 600_000)
        .await;
    let op = GuestFsOp::Write {
        path: gp("/workspace/f.txt"),
        body_b64: support::base64_encode(b"fuzz"),
        expected_sha256: None,
        create_parents: true,
    };
    let d = RequestDigest::of_canonical_json(&canonical_body(&op).unwrap());
    let grant_w = h.grant_w(
        &tid("alice"),
        &sid("sbx"),
        SandboxCapability::SandboxWrite,
        d.clone(),
        vec![gp("/workspace/f.txt")],
        None,
        Some(task.clone()),
        Some(lease.clone()),
    );
    let templates: Vec<Value> = vec![
        serde_json::to_value(&HostToWorker::InspectVm {
            envelope: env.clone(),
        })
        .unwrap(),
        serde_json::to_value(&HostToWorker::GetCapabilities {}).unwrap(),
        serde_json::to_value(&HostToWorker::FsList {
            grant: h.grant(
                &tid("alice"),
                &sid("sbx"),
                SandboxCapability::SandboxRead,
                d.clone(),
                vec![gp("/workspace")],
                None,
            ),
            path: gp("/workspace"),
            options: Default::default(),
        })
        .unwrap(),
        serde_json::to_value(&HostToWorker::FsWrite {
            grant: grant_w.clone(),
            path: gp("/workspace/f.txt"),
            body_b64: support::base64_encode(b"fuzz"),
            expected_sha256: None,
        })
        .unwrap(),
        serde_json::to_value(&HostToWorker::ReconcileQuery {}).unwrap(),
    ];

    // (a) Cross-tenant / identity splices: hostile edits of valid frames
    //     must be refused -- never Ok.
    fn auth_mut(v: &mut Value) -> Option<&mut Value> {
        if v.get("envelope").is_some() {
            v.get_mut("envelope")
        } else {
            v.get_mut("grant")
        }
    }
    let hostile_splices: Vec<(String, Value)> = templates
        .iter()
        .flat_map(|t| {
            if auth_mut(&mut t.clone()).is_none() {
                return Vec::new();
            }
            let mut bob = t.clone();
            if let Some(env) = auth_mut(&mut bob) {
                env["tenant_id"] = Value::String("bob".into());
            }
            let mut swapbox = t.clone();
            if let Some(env) = auth_mut(&mut swapbox) {
                env["sandbox_id"] = Value::String("victims-box".into());
            }
            let mut gen_bump = t.clone();
            if let Some(env) = auth_mut(&mut gen_bump)
                && let Some(g) = env.get_mut("instance_generation")
            {
                *g = Value::from(99u64);
            }
            let mut no_lease = t.clone();
            let stripped = match no_lease.get_mut("grant") {
                Some(gr) => {
                    gr["writer_lease"] = Value::Null;
                    true
                }
                None => false,
            };
            let mut out = vec![
                ("bob-tenant".to_string(), bob),
                ("foreign-sandbox".to_string(), swapbox),
                ("generation-bump".to_string(), gen_bump),
            ];
            if stripped {
                out.push(("lease-stripped".to_string(), no_lease));
            }
            out
        })
        .collect();
    for (label, body) in &hostile_splices {
        let reply = must_be_structured_reply(&h, 1000 + rng.below(1_000_000), body.clone()).await;
        match reply {
            WorkerToHost::Reply {
                outcome: agentkit_sandbox_contracts::api::ReplyOutcomeShape::Err { error },
                ..
            } => {
                // Splices must die on an authorization/consistency code,
                // not on an incidental parse error.
                assert!(
                    matches!(
                        error.code,
                        SandboxErrorCode::TenantForbidden
                            | SandboxErrorCode::PolicyDenied
                            | SandboxErrorCode::StaleGeneration
                            | SandboxErrorCode::NotFound
                            | SandboxErrorCode::Quarantined
                    ),
                    "{label}: unexpected error {error:?}"
                );
            }
            other => panic!("{label}: hostile frame accepted: {other:?}"),
        }
    }

    // (b) Byte-level corruption of the serialized templates: still exactly
    //     one structured reply, never a panic (parse failures included).
    for (i, t) in templates.iter().enumerate() {
        let s = serde_json::to_string(t).unwrap();
        for k in 0..40u64 {
            let mut bytes = s.clone().into_bytes();
            let pos = rng.below(bytes.len() as u64) as usize;
            bytes[pos] = bytes[pos] ^ (0x20 + (k % 60) as u8);
            if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                must_be_structured_reply(&h, 2000 + i as u64 * 100 + k, v).await;
            }
            must_be_structured_reply(&h, 2500 + i as u64 * 100 + k, rng.value(4)).await;
        }
    }

    // (c) Duplicate replay determinism: identical frame twice, identical
    //     outcome (replay or identical refusal -- never divergence).
    let dup_body = serde_json::to_value(&HostToWorker::InspectVm { envelope: env }).unwrap();
    let r1 = must_be_structured_reply(&h, 9000, dup_body.clone()).await;
    let r2 = must_be_structured_reply(&h, 9000, dup_body).await;
    let strip = |r: &WorkerToHost| match r {
        WorkerToHost::Reply { outcome, .. } => serde_json::to_string(outcome).unwrap(),
        other => format!("{other:?}"),
    };
    assert_eq!(strip(&r1), strip(&r2), "duplicate frame diverged");

    // (d) Post-fuzz health: the service still serves honest traffic.
    h.create_ready("alice", "postbox").await;
    let mut req = h.exec_request("post-fuzz", "true");
    let rd = support::digest_of(&req);
    req.request_digest = rd.clone();
    let body = h
        .ok(
            "post-fuzz",
            777,
            &HostToWorker::ExecStart {
                grant: h.grant(
                    &tid("alice"),
                    &sid("postbox"),
                    SandboxCapability::ProcessExec,
                    rd,
                    vec![gp("/workspace")],
                    Some(gp("/workspace")),
                ),
                request: Box::new(req),
            },
        )
        .await;
    assert!(
        matches!(
            body,
            agentkit_sandbox_contracts::api::messages::WorkerReplyBody::ExecStarted { .. }
        ),
        "post-fuzz exec: {body:?}"
    );
}
