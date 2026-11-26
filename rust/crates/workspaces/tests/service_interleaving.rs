//! Seeded random interleavings of calls, redeliveries, faults, stalls,
//! lifecycle actions and restarts (the WP-1.5 property test). After every
//! step: INV-3 (no operation id dispatched twice), INV-4 (at most one open
//! mutating operation per workspace, in the projection and in the guest)
//! and INV-5 (a generation is left behind only with a stop proof). No new
//! dependency: a tiny xorshift generator drives the choices; the runtime is
//! tokio's current-thread one, so a seed replays the same schedule.

mod support;

use std::collections::HashMap;
use std::sync::Arc;

use agentkit_workspaces::{
    ExecRequest, OperationId, OperationOutcome, OperationPhase, OutputLimit, RepositoryError,
    SandboxError, Settlement, WorkspaceRepository, WorkspaceService, WorkspaceState,
};
use support::{CURL, ECHO, EXIT_3, LONG, MARK, SLEEP, SLOW, TRUE, World, ctx, exec, home};
use tokio::task::JoinHandle;

type Task = JoinHandle<(String, Result<OperationOutcome, SandboxError>)>;

const SEEDS: [u64; 16] = [
    1, 2, 3, 5, 8, 13, 21, 34, 55, 89, 144, 233, 377, 610, 987, 1597,
];
const STEPS: usize = 80;
const AGENTS: [&str; 2] = ["a1", "a2"];
const COMMANDS: [&str; 7] = [TRUE, ECHO, EXIT_3, SLOW, LONG, MARK, CURL];

/// xorshift64*: enough randomness for schedules, fully determined by the
/// seed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

/// One call the schedule issued, kept for redelivery.
#[derive(Clone)]
struct Call {
    op: String,
    agent: &'static str,
    request: ExecRequest,
}

struct Run {
    world: World,
    svc: Arc<WorkspaceService>,
    rng: Rng,
    tasks: Vec<Task>,
    calls: Vec<Call>,
    /// How often each operation id was delivered.
    deliveries: HashMap<String, u32>,
}

impl Run {
    fn new(seed: u64) -> Self {
        let world = World::new();
        let (svc, _) = world.service();
        Self {
            world,
            svc,
            rng: Rng::new(seed),
            tasks: Vec::new(),
            calls: Vec::new(),
            deliveries: HashMap::new(),
        }
    }

    fn spawn(&mut self, call: Call) {
        let svc = self.svc.clone();
        let context = ctx(&call.op, call.agent);
        let binding = home(call.agent);
        *self.deliveries.entry(call.op.clone()).or_default() += 1;
        self.tasks.push(tokio::spawn(async move {
            let done = svc.exec(&context, &binding, call.request).await;
            (call.op, done)
        }));
    }

    /// Check the result of a call that ended.
    fn check_ended(&self, ended: Option<(String, Result<OperationOutcome, SandboxError>)>) {
        let Some((op, done)) = ended else {
            return;
        };
        let deliveries = self.deliveries.get(&op).copied().unwrap_or_default();
        let dispatched = self
            .world
            .fake
            .dispatch_calls(&OperationId::new(op.as_str()));
        check_result(&op, done, dispatched, deliveries);
    }

    fn new_call(&mut self) -> Call {
        let op = format!("op-{}", self.calls.len());
        let agent = *self.rng.pick(&AGENTS);
        let command = *self.rng.pick(&COMMANDS);
        // A small log limit keeps an injected endless printer cheap.
        let mut request = ExecRequest {
            output_limit: OutputLimit {
                max_bytes: 256 * 1024,
                max_lines: None,
            },
            ..exec(command)
        };
        if self.rng.below(4) == 0 {
            request.command = SLEEP.to_owned();
            request.timeout_ms = Some(3_000);
        }
        let call = Call { op, agent, request };
        self.calls.push(call.clone());
        call
    }

    fn arm_fault(&mut self) {
        let fake = &self.world.fake;
        match self.rng.below(8) {
            0 => fake.lose_next_ack(),
            1 => fake.hang_next_dispatch(),
            2 => fake.die_after_next_dispatch(),
            3 => fake.endless_next_exec(),
            4 => fake.refuse_stop(true),
            5 => fake.panic_next_dispatch(),
            _ => fake,
        };
    }

    /// A host crash: every in-flight call is dropped mid-wait; a new
    /// service recovers.
    async fn restart(&mut self) {
        for task in std::mem::take(&mut self.tasks) {
            task.abort();
            self.check_ended(task.await.ok());
        }
        self.svc = self.world.service().0;
        self.svc.recover_all().await.expect("recover_all");
    }

    async fn lifecycle(&mut self) {
        let agent = *self.rng.pick(&AGENTS);
        let Some(ws) = self
            .world
            .repo
            .workspace_by_key(&home(agent).key)
            .await
            .unwrap()
        else {
            return;
        };
        let outcome = match self.rng.below(4) {
            0 => self.svc.stop(&ws.id).await.map(drop),
            1 => self.svc.recover(&ws.id).await.map(drop),
            2 => self.svc.evict_idle(self.world.now()).await.map(drop),
            _ => {
                self.world.fake.refuse_stop(false);
                Ok(())
            }
        };
        if let Err(error) = outcome {
            assert!(refusal(&error), "unexpected lifecycle error: {error:?}");
        }
    }

    async fn step(&mut self) {
        match self.rng.below(12) {
            0..=3 => {
                if self.rng.below(4) == 0 {
                    self.arm_fault();
                }
                let call = self.new_call();
                self.spawn(call);
            }
            4 if !self.calls.is_empty() => {
                let call = self.rng.pick(&self.calls).clone();
                self.spawn(call);
            }
            5 => self.restart().await,
            6 | 7 => self.lifecycle().await,
            // A stall: long enough, at times, for leases to expire.
            8 => self.world.clock.advance(1 + self.rng.below(90_000) as i64),
            _ => self.world.clock.advance(self.rng.below(2_000) as i64),
        }
        for _ in 0..self.rng.below(40) {
            tokio::task::yield_now().await;
        }
        self.reap().await;
        self.check().await;
    }

    /// Check the result of every call that ended. A panicked task (an
    /// injected dispatch panic) is a crash, not a result.
    async fn reap(&mut self) {
        let (done, running): (Vec<Task>, Vec<Task>) =
            self.tasks.drain(..).partition(JoinHandle::is_finished);
        self.tasks = running;
        for task in done {
            self.check_ended(task.await.ok());
        }
    }

    async fn check(&self) {
        for call in &self.calls {
            let id = OperationId::new(call.op.as_str());
            assert!(
                self.world.fake.dispatch_calls(&id) <= 1,
                "INV-3: {id} dispatched twice"
            );
        }
        let open = self.world.repo.list_open_operations().await.unwrap();
        for agent in AGENTS {
            let Some(ws) = self
                .world
                .repo
                .workspace_by_key(&home(agent).key)
                .await
                .unwrap()
            else {
                continue;
            };
            let open_rows = open.iter().filter(|op| op.workspace_id == ws.id).count();
            assert!(open_rows <= 1, "INV-4: {open_rows} open rows in {}", ws.id);
            assert!(
                self.guest_running(&ws.id) <= 1,
                "INV-4: two guest operations in {}",
                ws.id
            );
        }
        let unproven = self.world.generations_proven().await;
        assert!(unproven.is_empty(), "INV-5: {unproven:?}");
    }

    fn guest_running(&self, ws: &agentkit_workspaces::WorkspaceId) -> usize {
        let mut running = 0;
        for (owner, generation) in self.world.fake.live_machines() {
            if &owner != ws {
                continue;
            }
            for call in &self.calls {
                let id = OperationId::new(call.op.as_str());
                let state = self.world.fake.operation_state(&owner, generation, &id);
                running += usize::from(state.is_some_and(|state| !state.is_terminal()));
            }
        }
        running
    }

    /// Let everything finish, restart, recover every fenced workspace and
    /// prove each workspace still takes a command.
    async fn drain(&mut self) {
        self.world.fake.refuse_stop(false);
        for _ in 0..5_000 {
            if self.tasks.iter().all(JoinHandle::is_finished) {
                break;
            }
            self.world.clock.advance(500);
            tokio::task::yield_now().await;
        }
        self.restart().await;
        assert!(
            self.world
                .repo
                .list_open_operations()
                .await
                .unwrap()
                .is_empty()
        );
        for agent in AGENTS {
            let binding = home(agent);
            if let Some(ws) = self
                .world
                .repo
                .workspace_by_key(&binding.key)
                .await
                .unwrap()
                && matches!(ws.state, WorkspaceState::Fenced { .. })
            {
                self.svc
                    .recover(&ws.id)
                    .await
                    .expect("recover after the faults stop");
            }
            self.settles(agent).await;
        }
        self.check().await;
    }

    /// The workspace of `agent` still takes a command and settles it. A
    /// fault armed during the run and never consumed fires first, so a
    /// few fresh calls may be needed.
    async fn settles(&mut self, agent: &'static str) {
        for attempt in 0..10 {
            let call = Call {
                op: format!("final-{agent}-{attempt}"),
                agent,
                request: exec(TRUE),
            };
            self.calls.push(call.clone());
            self.spawn(call);
            let task = self.tasks.pop().expect("just spawned");
            let Ok((_, done)) = task.await else {
                // An armed dispatch panic: the host crashed; restart.
                self.restart().await;
                continue;
            };
            {
                let done = done.expect("a quiet workspace refuses nothing");
                if matches!(done.settlement, Settlement::Settled(_)) {
                    return;
                }
                let binding = home(agent);
                if let Some(ws) = self
                    .world
                    .repo
                    .workspace_by_key(&binding.key)
                    .await
                    .unwrap()
                    && matches!(ws.state, WorkspaceState::Fenced { .. })
                {
                    self.svc.recover(&ws.id).await.expect("recover");
                }
            }
        }
        panic!("{agent}'s workspace never settled a command after the run");
    }
}

/// The refusals a schedule like this can cause: a held lease, a fence, a
/// machine that cannot start, a lease that expired during a stall before
/// the call wrote its row.
fn refusal(error: &SandboxError) -> bool {
    matches!(
        error,
        SandboxError::Busy { .. }
            | SandboxError::Fenced { .. }
            | SandboxError::Unavailable { .. }
            | SandboxError::Repository(RepositoryError::LeaseLost { .. })
    )
}

/// Every call ends in a receipt, an uncertain outcome or a refusal, never
/// in another error. A lost lease is a refusal only before the call wrote
/// its row: then nothing was dispatched, unless another delivery of the
/// same id did it (after a dispatch the call answers uncertain instead).
fn check_result(
    op: &str,
    done: Result<OperationOutcome, SandboxError>,
    dispatched: u32,
    deliveries: u32,
) {
    let Err(error) = done else {
        return;
    };
    assert!(refusal(&error), "{op}: unexpected error: {error:?}");
    if let SandboxError::Repository(RepositoryError::LeaseLost { .. }) = error {
        assert!(
            dispatched == 0 || deliveries > 1,
            "{op} was dispatched by its only delivery, which then answered {error:?}"
        );
    }
}

#[tokio::test]
async fn random_interleavings_keep_inv_3_4_and_5() {
    let (mut uncertain, mut advanced) = (0, 0);
    for seed in SEEDS {
        let mut run = Run::new(seed);
        for _ in 0..STEPS {
            run.step().await;
        }
        run.drain().await;
        assert!(
            run.world.fake.dispatch_calls_total() > 0,
            "seed {seed} dispatched nothing"
        );
        for agent in AGENTS {
            let key = home(agent).key;
            let Some(ws) = run.world.repo.workspace_by_key(&key).await.unwrap() else {
                continue;
            };
            advanced += usize::from(ws.generation.0 > 1);
            let rows = run.world.repo.list_operations(&ws.id, 1_000).await.unwrap();
            uncertain += rows
                .iter()
                .filter(|row| row.phase == OperationPhase::Uncertain)
                .count();
        }
    }
    // The schedules must reach the paths under test, not only receipts.
    assert!(
        uncertain > 0 && advanced > 0,
        "uncertain {uncertain}, advanced {advanced}"
    );
}
