use std::collections::BTreeMap;
use std::path::PathBuf;

use agentkit_sandbox_proto::launcher::{LauncherErrorCode, Tier};

use super::*;
use crate::fake::{DIGEST, FakeRuntime, scratch, test_config};
use crate::runtime::VolumeInfo;
use crate::token::valid_token;

struct Harness {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    config: Arc<Config>,
    fake: Arc<FakeRuntime>,
    /// `None` only inside `restart`: the old launcher must release the
    /// state directory before the new one locks it.
    service: Option<Service>,
}

impl Harness {
    fn new() -> Self {
        let (tmp, root) = scratch();
        let config = Arc::new(test_config(&root));
        let fake = Arc::new(FakeRuntime::default());
        let service = Some(Self::service(&config, &fake, &root));
        Self {
            _tmp: tmp,
            root,
            config,
            fake,
            service,
        }
    }

    fn service(config: &Arc<Config>, fake: &Arc<FakeRuntime>, root: &std::path::Path) -> Service {
        let state = StateStore::open(&config.state_dir).unwrap();
        let runtime: Arc<dyn RuntimeApi> = Arc::clone(fake) as Arc<dyn RuntimeApi>;
        Service::new(Arc::clone(config), runtime, state, TrustRoot::at(root))
    }

    /// A new launcher process over the same state directory and runtime.
    fn restart(&mut self) {
        self.service = None;
        self.service = Some(Self::service(&self.config, &self.fake, &self.root));
    }

    /// The same launcher with another configuration (same state and
    /// runtime).
    fn reconfigure(&mut self, change: impl FnOnce(&mut Config)) {
        let mut config = (*self.config).clone();
        change(&mut config);
        self.config = Arc::new(config);
        self.restart();
    }

    fn launcher(&self) -> &Service {
        self.service.as_ref().unwrap()
    }

    async fn ensure(&self, ws: &str, generation: u64) -> Reply {
        self.launcher()
            .handle(Request::Ensure {
                workspace_id: ws.into(),
                generation,
                image_digest: DIGEST.into(),
                tier: tier(),
            })
            .await
    }

    async fn stop(&self, ws: &str, generation: u64) -> Reply {
        self.stop_with_grace(ws, generation, 5_000).await
    }

    async fn stop_with_grace(&self, ws: &str, generation: u64, grace_ms: u64) -> Reply {
        self.launcher()
            .handle(Request::Stop {
                workspace_id: ws.into(),
                generation,
                grace_ms,
            })
            .await
    }

    async fn simple(&self, request: Request) -> Reply {
        self.launcher().handle(request).await
    }

    fn container_id(&self, name: &str) -> String {
        self.fake.with(|s| {
            s.containers
                .iter()
                .find(|c| c.name == name)
                .map(|c| c.id.clone())
                .unwrap()
        })
    }
}

fn tier() -> Tier {
    Tier {
        cpus_milli: 2000,
        memory_bytes: 2 << 30,
        pids: 512,
    }
}

fn ensured(reply: Reply) -> String {
    ensured_with_token(reply).0
}

/// The container id and channel token of an `ensured` reply.
fn ensured_with_token(reply: Reply) -> (String, String) {
    match reply {
        Reply::Ensured {
            container_id,
            channel_token,
        } => {
            assert!(valid_token(&channel_token));
            (container_id, channel_token)
        }
        other => panic!("expected ensured, got {other:?}"),
    }
}

fn code(reply: &Reply) -> LauncherErrorCode {
    match reply {
        Reply::Error(error) => error.code,
        other => panic!("expected an error, got {other:?}"),
    }
}

fn proven(reply: &Reply) -> bool {
    match reply {
        Reply::Stopped { proven, .. } => *proven,
        other => panic!("expected stopped, got {other:?}"),
    }
}

fn stale(current: u64) -> LauncherErrorCode {
    LauncherErrorCode::StaleGeneration { current }
}

mod machines;
mod storage;
