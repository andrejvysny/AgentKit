use super::*;
use agentkit_connections::{
    Connection, ConnectionConfig, ConnectionId, RepositoryMutation, Secret, SecretKey,
    encode_header_secret,
};
use std::collections::BTreeMap;

pub(super) struct SecretPlan {
    credential: Option<Secret>,
    headers: Option<Secret>,
    old: Option<(SecretKey, SecretKey)>,
    copy_credential: bool,
    copy_headers: bool,
}

impl DesktopBackend {
    pub(super) async fn complete_provider_write(
        &self,
        mutation: RepositoryMutation,
        record: Connection,
        plan: SecretPlan,
        status: u16,
    ) -> Result<OperationResponse, Problem> {
        let result = match self.provision_provider_secrets(&mutation, plan).await {
            Ok(()) => self
                .provider_value(record)
                .await
                .map(|value| output(status, value)),
            Err(error) => Err(error),
        };
        mutation.finish().map_err(providers::connection_error)?;
        result
    }
    pub(super) fn prepare_provider_secrets(
        &self,
        old: Option<&Connection>,
        config: &ConnectionConfig,
        body: &Value,
    ) -> Result<SecretPlan, Problem> {
        let credential = body.get("apiKey").and_then(Value::as_str).map(Secret::new);
        let headers = body
            .get("extraHeaders")
            .map(|headers| {
                let values: BTreeMap<_, _> = headers
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(name, value)| (name.clone(), Secret::new(value.as_str().unwrap())))
                    .collect();
                encode_header_secret(&config.extra_header_names, &values)
                    .map_err(providers::connection_error)
            })
            .transpose()?;
        let same_account = old.is_some_and(|old| old.config.account == config.account);
        Ok(SecretPlan {
            credential,
            headers: headers.filter(|_| !config.extra_header_names.is_empty()),
            old: old
                .map(|old| self.provider_secret_keys(&old.id, old.generation))
                .transpose()?
                .flatten(),
            copy_credential: same_account
                && config.credential_required
                && body.get("apiKey").is_none(),
            copy_headers: same_account
                && !config.extra_header_names.is_empty()
                && body.get("extraHeaders").is_none(),
        })
    }

    pub(super) fn provider_secret_keys(
        &self,
        id: &ConnectionId,
        generation: u64,
    ) -> Result<Option<(SecretKey, SecretKey)>, Problem> {
        self.native_connections
            .as_ref()
            .map(|native| {
                Ok((
                    native
                        .credential_key_at(id, generation)
                        .map_err(providers::connection_error)?,
                    native
                        .header_key_at(id, generation)
                        .map_err(providers::connection_error)?,
                ))
            })
            .transpose()
    }

    pub(super) async fn provision_provider_secrets(
        &self,
        mutation: &RepositoryMutation,
        plan: SecretPlan,
    ) -> Result<(), Problem> {
        let Some(secrets) = &self.secrets else {
            return if plan.copy_credential
                || plan.copy_headers
                || plan.credential.is_some()
                || plan.headers.is_some()
            {
                Err(secret_write_failed())
            } else {
                Ok(())
            };
        };
        let credential = if plan.copy_credential {
            carry(secrets, plan.old.as_ref().map(|keys| &keys.0)).await?
        } else {
            plan.credential
        };
        let headers = if plan.copy_headers {
            carry(secrets, plan.old.as_ref().map(|keys| &keys.1)).await?
        } else {
            plan.headers
        };
        if let Some(secret) = credential {
            secrets
                .put(
                    &mutation.credential_key().ok_or_else(secret_write_failed)?,
                    secret,
                )
                .await
                .map_err(|_| secret_write_failed())?;
        }
        if let Some(secret) = headers {
            secrets
                .put(
                    &mutation.header_key().ok_or_else(secret_write_failed)?,
                    secret,
                )
                .await
                .map_err(|_| secret_write_failed())?;
        }
        self.retire_provider_secrets(plan.old).await
    }

    pub(super) async fn retire_provider_secrets(
        &self,
        keys: Option<(SecretKey, SecretKey)>,
    ) -> Result<(), Problem> {
        if let (Some(secrets), Some((credential, headers))) = (&self.secrets, keys) {
            secrets
                .delete(&credential)
                .await
                .map_err(|_| secret_write_failed())?;
            secrets
                .delete(&headers)
                .await
                .map_err(|_| secret_write_failed())?;
        }
        Ok(())
    }
}

async fn carry(
    secrets: &agentkit_connections::SharedSecretStore,
    key: Option<&SecretKey>,
) -> Result<Option<Secret>, Problem> {
    let Some(key) = key else {
        return Err(secret_write_failed());
    };
    match secrets.get(key).await {
        Ok(secret) => Ok(Some(secret)),
        Err(agentkit_connections::ConnectionError::SecretMissing) => Ok(None),
        Err(_) => Err(secret_write_failed()),
    }
}

fn secret_write_failed() -> Problem {
    Problem::new(
        500,
        "secret_write_failed",
        "Connection metadata is committed. Retry PATCH with the intended apiKey/extraHeaders to provision its current generation. No older credential is used as a fallback; obsolete generation slots may remain after failure or cancellation.",
    )
}
