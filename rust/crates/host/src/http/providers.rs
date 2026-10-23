use super::*;
use agentkit_connections::{Connection, ConnectionError, ConnectionId};
use serde_json::json;
use std::collections::BTreeSet;

impl DesktopBackend {
    pub(super) async fn providers(
        &self,
        input: &OperationRequest,
    ) -> Result<OperationResponse, Problem> {
        match input.operation.as_str() {
            "listProviders" => {
                let mut values = Vec::new();
                for connection in self.connections.list().map_err(connection_error)? {
                    values.push(self.provider_value(connection).await?);
                }
                Ok(output(200, json!(values)))
            }
            "createProvider" => self.create_provider(input).await,
            "updateProvider" => self.update_provider(input).await,
            "deleteProvider" => self.delete_provider(input).await,
            "listModels" => {
                let id = ConnectionId::new(input.params["providerId"].clone())
                    .map_err(connection_error)?;
                self.connections.get(&id).map_err(connection_error)?;
                let catalog = self
                    .host
                    .store()
                    .get_setting("http.provider-catalog", id.as_str())
                    .await
                    .map_err(store_error)?
                    .unwrap_or_else(|| json!([]));
                Ok(output(200, catalog))
            }
            "testProvider" | "refreshProviderModels" => self.provider_operation(input).await,
            _ => Err(Problem::host(
                "internal_error",
                "Unknown provider operation",
            )),
        }
    }

    async fn create_provider(
        &self,
        input: &OperationRequest,
    ) -> Result<OperationResponse, Problem> {
        self.require_secret_port(&input.body)?;
        let id = super::chats::string(&input.body, "id")
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| self.host.runtime().id());
        let id = ConnectionId::new(id.trim()).map_err(connection_error)?;
        let config = super::provider_config::create(&input.body, self.allow_insecure_http)?;
        let plan = self.prepare_provider_secrets(None, &config, &input.body)?;
        let expected = self.connection_snapshot().await?;
        let mutation = self
            .connections
            .reserve_create(id.clone(), config)
            .map_err(|error| {
                if error == ConnectionError::AlreadyExists {
                    Problem::new(
                        409,
                        "duplicate_provider",
                        "A provider with this id already exists.",
                    )
                } else {
                    connection_error(error)
                }
            })?;
        let record = candidate_record(&mutation)?;
        let writes = vec![super::provider_commit::setting(
            "http.provider-model",
            id.as_str(),
            Some(input.body["defaultModel"].clone()),
        )];
        let mutation = self
            .commit_provider_mutation(mutation, expected, writes)
            .await?;
        self.complete_provider_write(mutation, record, plan, 201)
            .await
    }

    async fn update_provider(
        &self,
        input: &OperationRequest,
    ) -> Result<OperationResponse, Problem> {
        self.require_secret_port(&input.body)?;
        let id = ConnectionId::new(input.params["providerId"].clone()).map_err(connection_error)?;
        let old = self.connections.get(&id).map_err(connection_error)?;
        let config = super::provider_config::update(
            old.config.clone(),
            &input.body,
            self.allow_insecure_http,
        )?;
        let plan = self.prepare_provider_secrets(Some(&old), &config, &input.body)?;
        let expected = self.connection_snapshot().await?;
        let mutation = self
            .connections
            .reserve_update(&id, old.generation, config)
            .map_err(connection_error)?;
        let record = candidate_record(&mutation)?;
        let writes = input
            .body
            .get("defaultModel")
            .map(|model| {
                super::provider_commit::setting(
                    "http.provider-model",
                    id.as_str(),
                    Some(model.clone()),
                )
            })
            .into_iter()
            .collect();
        let mutation = self
            .commit_provider_mutation(mutation, expected, writes)
            .await?;
        self.complete_provider_write(mutation, record, plan, 200)
            .await
    }

    async fn delete_provider(
        &self,
        input: &OperationRequest,
    ) -> Result<OperationResponse, Problem> {
        let id = ConnectionId::new(input.params["providerId"].clone()).map_err(connection_error)?;
        let record = self.connections.get(&id).map_err(connection_error)?;
        let keys = self.provider_secret_keys(&id, record.generation)?;
        let expected = self.connection_snapshot().await?;
        let mutation = self
            .connections
            .reserve_delete(&id, record.generation)
            .map_err(connection_error)?;
        let writes = vec![
            super::provider_commit::setting("http.provider-model", id.as_str(), None),
            super::provider_commit::setting("http.provider-catalog", id.as_str(), None),
        ];
        let mutation = self
            .commit_provider_mutation(mutation, expected, writes)
            .await?;
        let result = self.retire_provider_secrets(keys).await;
        mutation.finish().map_err(connection_error)?;
        result?;
        Ok(output(204, Value::Null))
    }

    pub(super) async fn provider_value(&self, connection: Connection) -> Result<Value, Problem> {
        let model = self
            .host
            .store()
            .get_setting("http.provider-model", connection.id.as_str())
            .await
            .map_err(store_error)?
            .and_then(|v| v.as_str().map(str::to_owned))
            .or_else(|| connection.config.models.first().map(|m| m.id.clone()))
            .unwrap_or_default();
        let mut value = json!({"id":connection.id.as_str(),"label":connection.config.label,"kind":connection.config.provider_kind,
            "baseUrl":connection.config.endpoint.as_str().trim_end_matches('/'),"defaultModel":model,"enabled":connection.config.enabled});
        if connection.config.credential_required {
            // This compatibility reference is opaque; native secret keys remain app/account/generation-bound.
            value["apiKeySecretRef"] =
                json!(format!("provider/{}/api-key", connection.id.as_str()));
        }
        Ok(value)
    }
    fn require_secret_port(&self, body: &Value) -> Result<(), Problem> {
        if (body.get("apiKey").is_some() || !header_names(body).is_empty())
            && (self.native_connections.is_none() || self.secrets.is_none())
        {
            Err(optional(
                "This deployment has no connection secret store wired.",
            ))
        } else {
            Ok(())
        }
    }
}

fn candidate_record(
    mutation: &agentkit_connections::RepositoryMutation,
) -> Result<Connection, Problem> {
    mutation
        .connection()
        .cloned()
        .ok_or_else(|| Problem::host("internal_error", "Provider candidate has no connection."))
}

fn header_names(body: &Value) -> BTreeSet<String> {
    body.get("extraHeaders")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}
pub(super) fn connection_error(error: ConnectionError) -> Problem {
    match error {
        ConnectionError::NotFound => Problem::new(404, "not_found", error.to_string()),
        ConnectionError::StaleGeneration => {
            Problem::new(409, "revision_conflict", error.to_string())
        }
        ConnectionError::MutationPending => {
            Problem::new(503, "connection_mutation_pending", error.to_string())
        }
        ConnectionError::AuthorityPoisoned => {
            Problem::new(503, "connection_authority_poisoned", error.to_string())
        }
        ConnectionError::InvalidConfiguration
        | ConnectionError::InvalidIdentifier
        | ConnectionError::UnsupportedModel => {
            Problem::new(400, "invalid_request", error.to_string())
        }
        _ => Problem::host("internal_error", error.to_string()),
    }
}
