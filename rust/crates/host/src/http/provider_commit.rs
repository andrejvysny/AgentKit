use super::*;
use agentkit_connections::{RepositoryMutation, RepositorySnapshot};
use agentkit_store::{SettingWrite, SettingsCommit};
use serde_json::json;

impl DesktopBackend {
    pub(super) async fn provider_operation(
        &self,
        input: &OperationRequest,
    ) -> Result<OperationResponse, Problem> {
        let port = self.provider_operations.as_ref().ok_or_else(|| {
            optional("This deployment has no consent-backed provider operations wired.")
        })?;
        let id = &input.params["providerId"];
        let key =
            agentkit_connections::ConnectionId::new(id).map_err(providers::connection_error)?;
        let original = self
            .connections
            .get(&key)
            .map_err(providers::connection_error)?;
        let expected = self.connection_snapshot().await?;
        let value = if input.operation == "testProvider" {
            serde_json::to_value(port.test(&input.actor, id).await?)
        } else {
            serde_json::to_value(port.refresh_models(&input.actor, id).await?)
        }
        .map_err(|error| Problem::host("internal_error", error.to_string()))?;
        if input.operation == "refreshProviderModels" {
            let mutation = self
                .connections
                .reserve_snapshot()
                .map_err(providers::connection_error)?;
            if mutation
                .baseline()
                .connections
                .iter()
                .find(|connection| connection.id == key)
                != Some(&original)
            {
                mutation.abort().map_err(providers::connection_error)?;
                return Err(conflict());
            }
            let writes = vec![setting("http.provider-catalog", id, Some(value.clone()))];
            self.commit_provider_mutation(mutation, expected, writes)
                .await?
                .finish()
                .map_err(providers::connection_error)?;
        }
        Ok(output(200, value))
    }

    pub(super) async fn connection_snapshot(&self) -> Result<Option<Value>, Problem> {
        self.host
            .store()
            .get_setting("http", "connections")
            .await
            .map_err(store_error)
    }

    pub(super) async fn commit_provider_mutation(
        &self,
        mut mutation: RepositoryMutation,
        expected: Option<Value>,
        mut writes: Vec<SettingWrite>,
    ) -> Result<RepositoryMutation, Problem> {
        if !baseline_matches(
            expected.as_ref(),
            mutation.baseline(),
            self.connections.app_id().as_str(),
        ) {
            return Err(conflict());
        }
        let snapshot =
            json!({"appId":self.connections.app_id().as_str(),"repository":mutation.snapshot()});
        writes.push(setting("http", "connections", Some(snapshot)));
        let result = self
            .host
            .store()
            .compare_and_set_settings("http", "connections", expected, writes)
            .await;
        match result {
            Ok(SettingsCommit::Applied) => {
                mutation.publish().map_err(providers::connection_error)?;
                Ok(mutation)
            }
            Ok(SettingsCommit::Conflict) => Err(conflict()),
            Ok(SettingsCommit::Aborted(error)) => {
                mutation.abort().map_err(providers::connection_error)?;
                Err(store_error(error))
            }
            // Dropping an unpublished reservation poisons authority. A failed
            // commit acknowledgement cannot prove whether SQLite committed.
            Err(error) => Err(store_error(error)),
        }
    }
}

pub(super) fn setting(namespace: &str, key: &str, value: Option<Value>) -> SettingWrite {
    SettingWrite {
        namespace: namespace.into(),
        key: key.into(),
        value,
    }
}

fn conflict() -> Problem {
    Problem::new(
        409,
        "revision_conflict",
        "Durable connection metadata changed; reopen the connection authority.",
    )
}

fn baseline_matches(expected: Option<&Value>, baseline: &RepositorySnapshot, app: &str) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    if expected
        .get("appId")
        .is_some_and(|value| value.as_str() != Some(app))
    {
        return false;
    }
    let value = expected.get("repository").unwrap_or(expected).clone();
    let Ok(mut durable) = serde_json::from_value::<RepositorySnapshot>(value) else {
        return false;
    };
    durable
        .connections
        .sort_by(|a, b| (&a.app_id, &a.id).cmp(&(&b.app_id, &b.id)));
    // Other app namespaces may advance the shared authority's high-water mark.
    durable.version == baseline.version
        && durable.last_generation <= baseline.last_generation
        && durable.connections == baseline.connections
}
