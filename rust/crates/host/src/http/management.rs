use super::*;
use serde_json::json;

impl DesktopBackend {
    pub(super) async fn management(
        &self,
        input: &OperationRequest,
    ) -> Result<OperationResponse, Problem> {
        match input.operation.as_str() {
            "getVersion" => Ok(output(
                200,
                json!({"restApiVersion":"v1","contractVersion":"0.7.0","packages":{"host":"0.1.0"}}),
            )),
            "getSettings" | "updateSettings" => self.settings(input).await,
            "listProposals" => {
                let chat_id = &input.params["chatId"];
                if self
                    .host
                    .store()
                    .get_chat(chat_id)
                    .await
                    .map_err(store_error)?
                    .is_none()
                {
                    return Err(missing("Chat", chat_id));
                }
                let status = input
                    .query
                    .get("status")
                    .map(|s| serde_json::from_value(json!(s)))
                    .transpose()
                    .map_err(|_| {
                        Problem::new(400, "invalid_request", "Unknown proposal status.")
                    })?;
                let rows = self
                    .host
                    .store()
                    .proposals(Some(chat_id.clone()), status)
                    .await
                    .map_err(store_error)?;
                let mut items = Vec::new();
                for record in rows.into_iter().take(limit(input, 100)) {
                    items.push(self.project_proposal(record).await?);
                }
                Ok(output(200, json!(items)))
            }
            "approveProposal" | "rejectProposal" => {
                let service = self
                    .proposals
                    .as_ref()
                    .ok_or_else(|| optional("This deployment has no ProposalService wired."))?;
                let id = &input.params["proposalId"];
                let actor = serde_json::to_value(&input.actor)
                    .map_err(|e| Problem::host("internal_error", e.to_string()))?;
                let reason = input
                    .body
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let record = if input.operation == "approveProposal" {
                    service.approve_with_reason(id, &actor, reason).await
                } else {
                    service.reject(id, &actor, reason).await
                }
                .map_err(proposal_error)?;
                Ok(output(200, self.project_proposal(record).await?))
            }
            "listProviders"
            | "createProvider"
            | "updateProvider"
            | "deleteProvider"
            | "listModels"
            | "testProvider"
            | "refreshProviderModels" => self.providers(input).await,
            "applyProposal" => {
                let service = self
                    .proposals
                    .as_ref()
                    .ok_or_else(|| optional("This deployment has no ProposalService wired."))?;
                let id = &input.params["proposalId"];
                let actor = serde_json::to_value(&input.actor)
                    .map_err(|e| Problem::host("internal_error", e.to_string()))?;
                let receipt = service
                    .apply(id, input.body["operationId"].as_str().unwrap(), &actor)
                    .await
                    .map_err(proposal_error)?;
                let record = self
                    .host
                    .store()
                    .get_proposal(id)
                    .await
                    .map_err(store_error)?
                    .ok_or_else(|| missing("Proposal", id))?;
                let mut value = projections::proposal(record);
                value["outcome"] = json!({"status":receipt.outcome.status,"appliedOps":receipt.outcome.applied_ops,"failedOps":receipt.outcome.failed_ops});
                Ok(output(200, value))
            }
            "listAllowances" | "grantAllowance" | "revokeAllowance" => self.policy(input).await,
            "listMcpServers" | "createMcpServer" | "updateMcpServer" | "deleteMcpServer" => {
                self.mcp(input).await
            }
            "listTools" => {
                let port = self
                    .tool_catalog
                    .as_ref()
                    .ok_or_else(|| optional("This deployment has no tool catalog wired."))?;
                Ok(output(200, encoded(port.list(&input.actor).await?)?))
            }
            "listConnections"
            | "saveConnection"
            | "deleteConnection"
            | "connectionAction"
            | "listPromptTemplates"
            | "publishPromptTemplate"
            | "deletePromptTemplate"
            | "renderPromptTemplate"
            | "listSkills" => Err(optional(
                "Native managed content services are not configured.",
            )),
            _ => Err(Problem::host(
                "internal_error",
                "Unknown operation dispatch",
            )),
        }
    }

    async fn project_proposal(
        &self,
        record: agentkit_store::ProposalRecord,
    ) -> Result<Value, Problem> {
        let receipt = if let Some(id) = &record.operation_id {
            self.host.store().receipt(id).await.map_err(store_error)?
        } else {
            None
        };
        let mut value = projections::proposal(record);
        if let Some(receipt) = receipt {
            value["outcome"] = json!({"status":receipt.outcome.status,"appliedOps":receipt.outcome.applied_ops,"failedOps":receipt.outcome.failed_ops});
        }
        Ok(value)
    }

    async fn policy(&self, input: &OperationRequest) -> Result<OperationResponse, Problem> {
        let port = self
            .write_policy
            .as_ref()
            .ok_or_else(|| optional("This deployment has no write policy port wired."))?;
        let chat = &input.params["chatId"];
        if self
            .host
            .store()
            .get_chat(chat)
            .await
            .map_err(store_error)?
            .is_none()
        {
            return Err(missing("Chat", chat));
        }
        match input.operation.as_str() {
            "listAllowances" => Ok(output(
                200,
                json!({"allowances":port.list(&input.actor, chat).await?}),
            )),
            "grantAllowance" => {
                let request=serde_json::from_value(json!({"toolName":input.body["toolName"],"proposalKind":input.body["proposalKind"],"maxRisk":input.body["maxRisk"]}))
                    .map_err(|e|Problem::new(400,"invalid_request",e.to_string()))?;
                Ok(output(
                    201,
                    encoded(port.grant(&input.actor, chat, request).await?)?,
                ))
            }
            _ => {
                port.revoke(&input.actor, chat, &input.params["allowanceId"])
                    .await?;
                Ok(output(204, Value::Null))
            }
        }
    }

    async fn mcp(&self, input: &OperationRequest) -> Result<OperationResponse, Problem> {
        let port = self
            .mcp_configuration
            .as_ref()
            .ok_or_else(|| optional("This deployment has no MCP configuration port wired."))?;
        let mut body = input.body.clone();
        if let Some(alias) = body
            .get("alias")
            .and_then(Value::as_str)
            .map(|s| s.trim().to_owned())
        {
            body["alias"] = json!(alias);
        }
        let (status, mut value) = match input.operation.as_str() {
            "listMcpServers" => (200, encoded(port.list(&input.actor).await?)?),
            "createMcpServer" => {
                let request = serde_json::from_value(body.clone())
                    .map_err(|e| Problem::new(400, "invalid_request", e.to_string()))?;
                (201, encoded(port.create(&input.actor, request).await?)?)
            }
            "updateMcpServer" => {
                let request = serde_json::from_value(body.clone())
                    .map_err(|e| Problem::new(400, "invalid_request", e.to_string()))?;
                (
                    200,
                    encoded(
                        port.update(&input.actor, &input.params["serverId"], request)
                            .await?,
                    )?,
                )
            }
            _ => {
                port.delete(&input.actor, &input.params["serverId"]).await?;
                (204, Value::Null)
            }
        };
        redact_mcp(&mut value);
        Ok(output(status, value))
    }

    async fn settings(&self, input: &OperationRequest) -> Result<OperationResponse, Problem> {
        let mut value=self.host.store().get_setting("http","settings").await.map_err(store_error)?.unwrap_or_else(||json!({
            "contextSizePreference":"small","writePolicyMode":"auto_readonly_confirm_writes","allowRawToolData":false,"metadata":{}}));
        if input.operation == "updateSettings" {
            for key in [
                "defaultProviderId",
                "defaultModel",
                "contextSizePreference",
                "writePolicyMode",
                "allowRawToolData",
                "maxToolIterations",
                "toolCalling",
                "metadata",
            ] {
                if let Some(entry) = input.body.get(key) {
                    value[key] = entry.clone();
                }
            }
            self.host
                .store()
                .set_setting("http", "settings", value.clone())
                .await
                .map_err(store_error)?;
        }
        Ok(output(200, value))
    }
}

fn encoded(value: impl serde::Serialize) -> Result<Value, Problem> {
    serde_json::to_value(value).map_err(|e| Problem::host("internal_error", e.to_string()))
}
fn redact_mcp(value: &mut Value) {
    if let Value::Array(items) = value {
        for item in items {
            redact_mcp(item);
        }
        return;
    }
    for field in ["env", "headers"] {
        if let Some(values) = value
            .get_mut("transport")
            .and_then(|t| t.get_mut(field))
            .and_then(Value::as_object_mut)
        {
            for secret in values.values_mut() {
                *secret = json!("***");
            }
        }
    }
}

fn proposal_error(error: crate::HostError) -> Problem {
    match error {
        crate::HostError::Store(agentkit_store::StoreError::InvalidTransition(detail)) => {
            Problem::host("invalid_proposal_transition", detail)
        }
        _ => host_error(error),
    }
}
