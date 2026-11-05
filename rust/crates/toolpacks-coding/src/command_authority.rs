use super::{PackEnv, ToolContext, process_env, sanitized_env};
use crate::command_fingerprint::command_fingerprint;
use crate::errors::CodingError;
use crate::process::SpawnAuthorization;
use std::collections::HashMap;
use std::sync::Arc;

struct CommandAuthority {
    env: Arc<PackEnv>,
    context: ToolContext,
    root: String,
    cwd: String,
    command: String,
    environment: HashMap<String, String>,
    scratch: String,
    fingerprint: Option<String>,
}

pub(super) fn spawn_authorization(
    env: Arc<PackEnv>,
    context: ToolContext,
    root: String,
    cwd: String,
    command: String,
    environment: HashMap<String, String>,
    scratch: String,
) -> SpawnAuthorization {
    let fingerprint = command_fingerprint(&root, &cwd, &command, &environment)
        .ok()
        .flatten();
    let authority = Arc::new(CommandAuthority {
        env,
        context,
        root,
        cwd,
        command,
        environment,
        scratch,
        fingerprint,
    });
    Arc::new(move || {
        let authority = Arc::clone(&authority);
        Box::pin(async move { authority.check().await })
    })
}

impl CommandAuthority {
    async fn check(&self) -> Result<(), CodingError> {
        if let Some(guard) = &self.context.dispatch_guard {
            guard
                .check()
                .await
                .map_err(|error| CodingError::new(&error.code, error.message))?;
        }
        self.check_liveness()?;
        self.check_identity()?;
        // Reading startup files can outlast an approval or task lease.
        if let Some(guard) = &self.context.dispatch_guard {
            guard
                .check()
                .await
                .map_err(|error| CodingError::new(&error.code, error.message))?;
        }
        self.check_liveness()
    }

    fn check_identity(&self) -> Result<(), CodingError> {
        for expected in [&self.root, &self.cwd] {
            if std::fs::canonicalize(expected).ok().as_deref()
                != Some(std::path::Path::new(expected))
            {
                return Err(changed("command workspace or cwd binding changed"));
            }
        }
        let current = self
            .env
            .environment
            .as_deref()
            .cloned()
            .unwrap_or_else(process_env);
        if sanitized_env(&current, &self.scratch) != sanitized_env(&self.environment, &self.scratch)
        {
            return Err(changed("effective command environment changed"));
        }
        let current_identity = command_fingerprint(&self.root, &self.cwd, &self.command, &current)
            .ok()
            .flatten();
        // Unkeyable commands require fresh or unrestricted authority. A
        // remembered grant is independently rekeyed by the host guard and
        // fails closed if the identity becomes incomplete while waiting.
        if self.fingerprint.is_some() && current_identity != self.fingerprint {
            return Err(changed("command startup inputs changed"));
        }
        Ok(())
    }

    fn check_liveness(&self) -> Result<(), CodingError> {
        if self.context.cancellation.is_cancelled() || self.env.is_closed() {
            return Err(CodingError::new("cancelled", "command authority cancelled"));
        }
        Ok(())
    }
}

fn changed(reason: &str) -> CodingError {
    CodingError::new(
        "authorization_changed",
        format!("{reason}; fresh approval required"),
    )
}
