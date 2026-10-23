use crate::{
    ConnectionError, ConnectionId, ConsentToken, NativeConnections, RequestPlan, Secret, SecretKey,
};
use std::collections::{BTreeMap, BTreeSet};

/// Resolved only after approval; never serializable, printable, or part of a snapshot.
pub struct ResolvedSecrets {
    pub api_key: Option<Secret>,
    pub headers: BTreeMap<String, Secret>,
}

impl NativeConnections {
    pub fn header_key(&self, id: &ConnectionId) -> Result<SecretKey, ConnectionError> {
        Ok(self.credential_key(id)?.for_headers())
    }

    pub fn header_key_at(
        &self,
        id: &ConnectionId,
        expected_generation: u64,
    ) -> Result<SecretKey, ConnectionError> {
        Ok(self
            .credential_key_at(id, expected_generation)?
            .for_headers())
    }

    pub async fn provision_headers(
        &self,
        id: &ConnectionId,
        headers: BTreeMap<String, Secret>,
    ) -> Result<(), ConnectionError> {
        let generation = self.repository.get(id)?.generation;
        self.provision_headers_at(id, generation, headers).await
    }

    pub async fn provision_headers_at(
        &self,
        id: &ConnectionId,
        expected_generation: u64,
        headers: BTreeMap<String, Secret>,
    ) -> Result<(), ConnectionError> {
        let current = self.repository.get(id)?;
        if current.generation != expected_generation {
            return Err(ConnectionError::StaleGeneration);
        }
        let secret = encode_header_secret(&current.config.extra_header_names, &headers)?;
        self.secrets
            .put(&SecretKey::for_connection(&current).for_headers(), secret)
            .await?;
        if self.repository.get(id)? != current {
            return Err(ConnectionError::StaleGeneration);
        }
        Ok(())
    }

    pub(crate) async fn resolve_secrets(
        &self,
        plan: &RequestPlan,
        consent: &ConsentToken,
    ) -> Result<ResolvedSecrets, ConnectionError> {
        self.validate_consent(plan, consent)?;
        let connection = plan.connection();
        let key = SecretKey::for_connection(connection);
        let api_key = if connection.config.credential_required {
            Some(self.secrets.get(&key).await)
        } else {
            None
        };
        self.validate_consent(plan, consent)?;
        let api_key = api_key.transpose()?;
        let headers = if connection.config.extra_header_names.is_empty() {
            None
        } else {
            Some(self.secrets.get(&key.for_headers()).await)
        };
        self.validate_consent(plan, consent)?;
        let headers = headers
            .transpose()?
            .map(|secret| decode_headers(&secret, &connection.config.extra_header_names))
            .transpose()?
            .unwrap_or_default();
        Ok(ResolvedSecrets { api_key, headers })
    }
}

/// Validate and encode a host-prepared header map without reading credentials or
/// consulting a repository that may be reserved for a durable mutation.
pub fn encode_header_secret(
    expected: &BTreeSet<String>,
    headers: &BTreeMap<String, Secret>,
) -> Result<Secret, ConnectionError> {
    crate::types::validate_header_names(expected)?;
    if headers.keys().cloned().collect::<BTreeSet<_>>() != *expected {
        return Err(ConnectionError::InvalidConfiguration);
    }
    let values: BTreeMap<_, _> = headers
        .iter()
        .map(|(name, secret)| (name, secret.expose()))
        .collect();
    let encoded =
        serde_json::to_string(&values).map_err(|_| ConnectionError::InvalidConfiguration)?;
    let secret = Secret::new(encoded);
    decode_headers(&secret, expected)?;
    Ok(secret)
}

fn decode_headers(
    secret: &Secret,
    expected: &BTreeSet<String>,
) -> Result<BTreeMap<String, Secret>, ConnectionError> {
    if secret.expose().len() > 65536 {
        return Err(ConnectionError::InvalidConfiguration);
    }
    let values: BTreeMap<String, String> =
        serde_json::from_str(secret.expose()).map_err(|_| ConnectionError::InvalidConfiguration)?;
    if values.keys().cloned().collect::<BTreeSet<_>>() != *expected
        || values.values().any(|value| {
            value.len() > 8192
                || value
                    .bytes()
                    .any(|byte| byte == b'\r' || byte == b'\n' || byte == 0)
        })
    {
        return Err(ConnectionError::InvalidConfiguration);
    }
    Ok(values
        .into_iter()
        .map(|(name, value)| (name, Secret::new(value)))
        .collect())
}
