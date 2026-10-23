use crate::*;
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeResult {
    pub reachable: bool,
    pub http_status: u16,
}

impl NativeConnections {
    /// Preparing discovery performs no credential reads or network I/O.
    pub fn plan_discovery(&self, id: &ConnectionId) -> Result<RequestPlan, ConnectionError> {
        self.plan_catalogue_request(id, Capability::ModelDiscovery)
    }

    pub fn plan_probe(&self, id: &ConnectionId) -> Result<RequestPlan, ConnectionError> {
        self.plan_catalogue_request(id, Capability::Probe)
    }

    fn plan_catalogue_request(
        &self,
        id: &ConnectionId,
        capability: Capability,
    ) -> Result<RequestPlan, ConnectionError> {
        let mut destination = self.repository.get(id)?.config.endpoint;
        destination.set_path(&format!(
            "{}/models",
            destination.path().trim_end_matches('/')
        ));
        self.plan_to(
            id,
            PlanInput {
                capability,
                model: None,
                disclosed_data: BTreeSet::from(["connection-authentication".into()]),
                payload: vec![],
            },
            destination,
        )
    }

    /// Returns candidates only; callers explicitly select and persist a model.
    pub async fn discover_models(
        &self,
        plan: &RequestPlan,
        consent: &ConsentToken,
    ) -> Result<Vec<Model>, ConnectionError> {
        if plan.capability() != &Capability::ModelDiscovery {
            return Err(ConnectionError::UnsupportedCapability);
        }
        let response = self.execute(plan, consent).await?;
        if !(200..300).contains(&response.status) {
            return Err(ConnectionError::NetworkFailed);
        }
        parse_models(&response.body, &plan.connection().config.capabilities)
    }

    /// An authenticated catalogue response proves reachability, not inference/tool support.
    pub async fn probe(
        &self,
        plan: &RequestPlan,
        consent: &ConsentToken,
    ) -> Result<ProbeResult, ConnectionError> {
        if plan.capability() != &Capability::Probe {
            return Err(ConnectionError::UnsupportedCapability);
        }
        let response = self.execute(plan, consent).await?;
        Ok(ProbeResult {
            reachable: (200..300).contains(&response.status),
            http_status: response.status,
        })
    }
}

fn parse_models(
    body: &[u8],
    supported: &BTreeSet<Capability>,
) -> Result<Vec<Model>, ConnectionError> {
    let text = std::str::from_utf8(body).map_err(|_| ConnectionError::NetworkFailed)?;
    let value = agentkit_contracts::parse_javascript_json(text)
        .map_err(|_| ConnectionError::NetworkFailed)?;
    let items = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or(ConnectionError::NetworkFailed)?;
    if items.len() > 10_000 {
        return Err(ConnectionError::NetworkFailed);
    }
    let capabilities = supported
        .iter()
        .filter(|capability| !matches!(capability, Capability::Probe | Capability::ModelDiscovery))
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut ids = BTreeSet::new();
    let mut models = Vec::with_capacity(items.len());
    for item in items {
        let id = item["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or(ConnectionError::NetworkFailed)?;
        if !ids.insert(id) {
            return Err(ConnectionError::NetworkFailed);
        }
        let display_name = match item.get("name") {
            None | Some(Value::Null) => None,
            Some(Value::String(name)) => Some(name.clone()),
            _ => return Err(ConnectionError::NetworkFailed),
        };
        models.push(Model {
            id: id.into(),
            display_name,
            capabilities: capabilities.clone(),
            context_window_tokens: None,
        });
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_unknown_fields_keep_literal_private_number_objects() {
        let body =
            br#"{"data":[{"id":"model","metadata":{"$serde_json::private::Number":"literal"}}]}"#;
        let models = parse_models(body, &BTreeSet::from([Capability::Chat])).unwrap();
        assert_eq!(models[0].id, "model");
    }
}
