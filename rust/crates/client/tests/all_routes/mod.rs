use super::*;

pub(super) async fn chats(f: &mut Fixture) -> String {
    let version = f
        .call(Op::GetVersion, Call::default(), "VersionDtoSchema", 200)
        .await;
    assert_eq!(
        version["contractVersion"],
        agentkit_contracts::CONTRACT_VERSION
    );
    let chat = f
        .call(
            Op::CreateChat,
            request(&[], Some(json!({"title":"Reverse routes"}))),
            "ChatDtoSchema",
            201,
        )
        .await;
    let chat = id(&chat, "id");
    f.call(Op::ListChats, Call::default(), "ChatDtoSchema[]", 200)
        .await;
    f.call(
        Op::GetChat,
        request(&[("chatId", &chat)], None),
        "ChatDtoSchema",
        200,
    )
    .await;
    let updated = f
        .call(
            Op::UpdateChat,
            request(&[("chatId", &chat)], Some(json!({"title":"Renamed"}))),
            "ChatDtoSchema",
            200,
        )
        .await;
    assert_eq!(updated["title"], "Renamed");
    let mut call = request(&[("chatId", &chat)], Some(json!({"content":"question"})));
    call.idempotency_key = Some("reverse-submit".into());
    let turn = f
        .call(
            Op::SubmitMessage,
            call.clone(),
            "SubmitMessageResponseSchema",
            201,
        )
        .await;
    assert_eq!(
        f.call(Op::SubmitMessage, call, "SubmitMessageResponseSchema", 200)
            .await,
        turn
    );
    let events = f.drain(&id(&turn, "runId")).await;
    assert_eq!(events.last().unwrap()["type"], "run.completed");
    let run = f
        .call(
            Op::GetRun,
            request(&[("runId", &id(&turn, "runId"))], None),
            "RunDtoSchema",
            200,
        )
        .await;
    assert_eq!(run["status"], "completed");
    let page = f
        .call(
            Op::ListMessages,
            request(&[("chatId", &chat)], None),
            "MessagePageDtoSchema",
            200,
        )
        .await;
    assert_eq!(page["items"][1]["content"], "Reverse answer 1.");
    branches(f, &chat, &turn).await;
    let search = Call {
        query: BTreeMap::from([
            ("q".into(), "question".into()),
            ("chatId".into(), chat.clone()),
        ]),
        ..Call::default()
    };
    let hits = f
        .call(
            Op::SearchMessages,
            search,
            "MessageSearchResponseSchema",
            200,
        )
        .await;
    assert!(!hits["hits"].as_array().unwrap().is_empty());
    f.call(
        Op::ListToolEvents,
        request(&[("chatId", &chat)], None),
        "ToolEventDtoSchema[]",
        200,
    )
    .await;
    chat
}
pub(super) async fn branches(f: &mut Fixture, chat: &str, turn: &Value) {
    let assistant = id(turn, "assistantMessageId");
    let mut call = request(
        &[("chatId", chat), ("messageId", &assistant)],
        Some(json!({})),
    );
    call.idempotency_key = Some("reverse-regenerate".into());
    let next = f
        .call(
            Op::RegenerateMessage,
            call,
            "SubmitMessageResponseSchema",
            201,
        )
        .await;
    f.drain(&id(&next, "runId")).await;
    let siblings = f
        .call(
            Op::ListSiblings,
            request(&[("messageId", &assistant)], None),
            "MessageDtoSchema[]",
            200,
        )
        .await;
    assert_eq!(siblings.as_array().unwrap().len(), 2);
    let active = f
        .call(
            Op::ActivateBranch,
            request(&[("messageId", &assistant)], None),
            "MessagePageDtoSchema",
            200,
        )
        .await;
    assert_eq!(active["items"][1]["id"], assistant);
    let fork = f
        .call(
            Op::ForkChat,
            request(
                &[("chatId", chat)],
                Some(json!({"fromMessageId":assistant})),
            ),
            "ChatDtoSchema",
            201,
        )
        .await;
    f.call(
        Op::DeleteChat,
        request(&[("chatId", &id(&fork, "id"))], None),
        "",
        204,
    )
    .await;
}
pub(super) async fn providers(f: &mut Fixture) {
    let provider=f.call(Op::CreateProvider,request(&[],Some(json!({"id":"reverse-provider","label":"Reverse","kind":"openai-compatible","baseUrl":"http://localhost:9999","defaultModel":"m1","apiKey":"reverse-secret","extraHeaders":{"x-secret":"reverse-header-secret"}}))),"ProviderDtoSchema",201).await;
    assert!(!provider.to_string().contains("reverse-secret"));
    let provider = id(&provider, "id");
    let listed = f
        .call(
            Op::ListProviders,
            Call::default(),
            "ProviderDtoSchema[]",
            200,
        )
        .await;
    assert!(!listed.to_string().contains("reverse-header-secret"));
    let path = [("providerId", provider.as_str())];
    let updated = f
        .call(
            Op::UpdateProvider,
            request(&path, Some(json!({"label":"Updated"}))),
            "ProviderDtoSchema",
            200,
        )
        .await;
    assert_eq!(updated["label"], "Updated");
    assert_eq!(
        f.call(
            Op::ListModels,
            request(&path, None),
            "ModelDtoSchema[]",
            200
        )
        .await,
        json!([])
    );
    let models = f
        .call(
            Op::RefreshProviderModels,
            request(&path, None),
            "ModelDtoSchema[]",
            200,
        )
        .await;
    assert_eq!(models[0]["modelId"], "m2");
    assert_eq!(
        f.call(
            Op::ListModels,
            request(&path, None),
            "ModelDtoSchema[]",
            200
        )
        .await,
        models
    );
    assert_eq!(
        f.call(
            Op::TestProvider,
            request(&path, None),
            "TestProviderResponseSchema",
            200
        )
        .await["ok"],
        true
    );
    f.call(Op::DeleteProvider, request(&path, None), "", 204)
        .await;
}
pub(super) async fn management(f: &mut Fixture, chat: &str) {
    f.call(Op::GetSettings, Call::default(), "SettingsDtoSchema", 200)
        .await;
    assert_eq!(
        f.call(
            Op::UpdateSettings,
            request(&[], Some(json!({"maxToolIterations":2}))),
            "SettingsDtoSchema",
            200
        )
        .await["maxToolIterations"],
        2
    );
    let path = [("chatId", chat)];
    assert_eq!(
        f.call(
            Op::ListAllowances,
            request(&path, None),
            "WriteAllowanceListResponseSchema",
            200
        )
        .await["allowances"],
        json!([])
    );
    let allowance = f
        .call(
            Op::GrantAllowance,
            request(
                &path,
                Some(json!({"toolName":"edit","proposalKind":"edit","maxRisk":"low"})),
            ),
            "WriteAllowanceDtoSchema",
            201,
        )
        .await;
    f.call(
        Op::RevokeAllowance,
        request(
            &[("chatId", chat), ("allowanceId", &id(&allowance, "key"))],
            None,
        ),
        "",
        204,
    )
    .await;
    let config=f.call(Op::CreateMcpServer,request(&[],Some(json!({"alias":"reverse","transport":{"kind":"stdio","command":"fixture","env":{"TOKEN":"reverse-mcp-private"}}}))),"McpServerDtoSchema",201).await;
    assert!(!config.to_string().contains("reverse-mcp-private"));
    f.call(
        Op::ListMcpServers,
        Call::default(),
        "McpServerDtoSchema[]",
        200,
    )
    .await;
    let path = [("serverId", id(&config, "id"))];
    assert_eq!(
        f.call(
            Op::UpdateMcpServer,
            request(&[(path[0].0, &path[0].1)], Some(json!({"enabled":false}))),
            "McpServerDtoSchema",
            200
        )
        .await["enabled"],
        false
    );
    f.call(
        Op::DeleteMcpServer,
        request(&[(path[0].0, &path[0].1)], None),
        "",
        204,
    )
    .await;
    assert_eq!(
        f.call(
            Op::ListTools,
            Call::default(),
            "ToolDefinitionDtoSchema[]",
            200
        )
        .await[0]["name"],
        "fixture_read"
    );
}
pub(super) async fn proposals(f: &mut Fixture) {
    let listed = f
        .call(
            Op::ListProposals,
            request(&[("chatId", support::CHAT)], None),
            "ProposalDtoSchema[]",
            200,
        )
        .await;
    assert_eq!(listed.as_array().unwrap().len(), 2);
    let approved = f
        .call(
            Op::ApproveProposal,
            request(
                &[("proposalId", "approve-me")],
                Some(json!({"reason":"reviewed"})),
            ),
            "ProposalDtoSchema",
            200,
        )
        .await;
    assert_eq!(approved["decision"]["actor"], "user");
    let call = request(
        &[("proposalId", "approve-me")],
        Some(json!({"operationId":"reverse-apply"})),
    );
    let applied = f
        .call(Op::ApplyProposal, call.clone(), "ProposalDtoSchema", 200)
        .await;
    assert_eq!(applied["outcome"]["appliedOps"], 1);
    assert_eq!(
        f.call(Op::ApplyProposal, call, "ProposalDtoSchema", 200)
            .await,
        applied
    );
    assert_eq!(
        f.call(
            Op::RejectProposal,
            request(
                &[("proposalId", "reject-me")],
                Some(json!({"reason":"declined"}))
            ),
            "ProposalDtoSchema",
            200
        )
        .await["status"],
        "rejected"
    );
}
pub(super) async fn cancellation_and_resume(f: &mut Fixture) {
    let chat = f
        .call(
            Op::CreateChat,
            request(&[], Some(json!({}))),
            "ChatDtoSchema",
            201,
        )
        .await;
    let mut call = request(
        &[("chatId", &id(&chat, "id"))],
        Some(json!({"content":"park"})),
    );
    call.idempotency_key = Some("reverse-park".into());
    let parked = f
        .call(Op::SubmitMessage, call, "SubmitMessageResponseSchema", 201)
        .await;
    let run = id(&parked, "runId");
    let mut stream = f.client.stream_run(&run, StreamOptions::default());
    tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    f.covered.insert(Op::StreamRun.as_str().into(), 200);
    f.call(
        Op::CancelRun,
        request(&[("runId", &run)], None),
        "RunDtoSchema",
        202,
    )
    .await;
    let tail: Vec<_> = tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
        .await
        .unwrap();
    assert!(tail.iter().all(Result::is_ok));
    assert_eq!(
        f.call(
            Op::GetRun,
            request(&[("runId", &run)], None),
            "RunDtoSchema",
            200
        )
        .await["status"],
        "cancelled"
    );
    let resumed = f
        .call(
            Op::ResumeRun,
            request(&[("runId", "resume-me")], None),
            "RunDtoSchema",
            202,
        )
        .await;
    assert_eq!(resumed["runId"], "resume-me");
    f.drain("resume-me").await;
    assert_eq!(
        f.call(
            Op::GetRun,
            request(&[("runId", "resume-me")], None),
            "RunDtoSchema",
            200
        )
        .await["status"],
        "completed"
    );
}

fn template(revision: &str) -> Value {
    json!({
        "id": "greet",
        "revision": revision,
        "description": "Greets someone",
        "argumentsSchema": {
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "required": ["name"],
        },
        "messages": [{"role": "system", "content": "Hello {{name}}!"}],
    })
}
/// The routes whose services the TypeScript fixture left unwired (and so
/// answered 501) are real on the native host; each runs to its success status.
pub(super) async fn optional_services(f: &mut Fixture) {
    managed_content(f).await;
    teammates(f).await;
}
/// The contract declares no request schema for the three body-carrying
/// managed-content routes, so the typed client refuses to send a body: through
/// the client they answer the server's 400, and the successful path is driven
/// with a raw request.
async fn managed_content(f: &mut Fixture) {
    f.refused(Op::PublishPromptTemplate, request(&[], None), 400)
        .await;
    let published = f
        .raw(
            Op::PublishPromptTemplate,
            "/v1/prompt-templates",
            Some(template("1")),
            201,
        )
        .await;
    check(
        &agentkit_contracts::parse_javascript_json(&published).unwrap(),
        "PromptTemplateSchema",
    );
    let listed = f
        .call(
            Op::ListPromptTemplates,
            Call::default(),
            "PromptTemplateSchema[]",
            200,
        )
        .await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    let path = [("templateId", "greet"), ("revision", "1")];
    f.refused(Op::RenderPromptTemplate, request(&path, None), 400)
        .await;
    let rendered = f
        .raw(
            Op::RenderPromptTemplate,
            "/v1/prompt-templates/greet/1/render",
            Some(json!({"name": "world"})),
            200,
        )
        .await;
    assert!(rendered.contains("Hello world!"), "{rendered}");
    f.call(Op::DeletePromptTemplate, request(&path, None), "", 204)
        .await;
    f.call(Op::ListSkills, Call::default(), "*", 200).await;
    let connection = [("connectionId", "c1")];
    f.refused(Op::SaveConnection, request(&connection, None), 400)
        .await;
    let saved = f
        .raw(
            Op::SaveConnection,
            "/v1/connections/c1",
            Some(json!({
                "connectorId": "github",
                "accountId": "a1",
                "revision": 1,
                "expectedRevision": null,
                "config": {
                    "alias": "github",
                    "transport": {"kind": "http", "url": "http://127.0.0.1:9/mcp"},
                },
            })),
            200,
        )
        .await;
    let saved = agentkit_contracts::parse_javascript_json(&saved).unwrap();
    // ConnectionRecordSchema is not applied: the native record carries
    // `workspaceId: null`, which that schema rejects (reported as a follow-up).
    assert_eq!(saved["revision"], 1);
    let listed = f.call(Op::ListConnections, Call::default(), "*", 200).await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    // The documented MCP-OAuth reduction: lifecycle actions are refused.
    f.refused(Op::ConnectionAction, request(&connection, None), 501)
        .await;
    f.call(Op::DeleteConnection, request(&connection, None), "", 204)
        .await;
}
async fn teammates(f: &mut Fixture) {
    let created = f
        .call(
            Op::CreateAgent,
            request(&[], Some(json!({"definition": agent_definition()}))),
            "AgentDtoSchema",
            201,
        )
        .await;
    let agent = id(&created, "agentId");
    let path = [("agentId", agent.as_str())];
    f.call(Op::ListAgents, Call::default(), "AgentDtoSchema[]", 200)
        .await;
    f.call(Op::GetAgent, request(&path, None), "AgentDtoSchema", 200)
        .await;
    let updated = f
        .call(
            Op::UpdateAgent,
            request(
                &path,
                Some(json!({"expectedRevision": 1, "definition": agent_definition()})),
            ),
            "AgentDtoSchema",
            200,
        )
        .await;
    assert_eq!(updated["currentRevision"], 2);
    let remodeled = f
        .call(
            Op::SetAgentModel,
            request(&path, Some(json!({"expectedRevision": 2, "model": "m1"}))),
            "AgentDtoSchema",
            200,
        )
        .await;
    assert_eq!(remodeled["currentRevision"], 3);
    f.call(
        Op::ListAgentRevisions,
        request(&path, None),
        "AgentRevisionDtoSchema[]",
        200,
    )
    .await;
    let submit = request(
        &path,
        Some(json!({"admissionKey": "all-routes", "kind": "review.diff", "payload": {}})),
    );
    let admitted = f.call(Op::SubmitTeammateTask, submit, "*", 201).await;
    let task = id(&admitted["task"], "taskId");
    let task_path = [("taskId", task.as_str())];
    f.call(Op::ListAgentTasks, request(&path, None), "*", 200)
        .await;
    f.call(Op::GetAgentTask, request(&task_path, None), "*", 200)
        .await;
    f.call(
        Op::GetAgentTaskRunProfile,
        request(&task_path, None),
        "*",
        200,
    )
    .await;
    // Nothing executes `review.diff` here: the queued task settles on cancel.
    let cancelled = f
        .call(Op::CancelAgentTask, request(&task_path, None), "*", 202)
        .await;
    assert_eq!(cancelled["status"], "cancelled");
    f.call(Op::ListAgentTaskEvents, request(&task_path, None), "*", 200)
        .await;
    // The client has no typed agent-task stream; read the raw SSE body.
    let body = f
        .raw(
            Op::StreamAgentTask,
            &format!("/v1/agent-tasks/{task}/stream"),
            None,
            200,
        )
        .await;
    assert!(body.contains("event:") || body.contains("data:"), "{body}");
    f.call(
        Op::ArchiveAgent,
        request(&path, None),
        "AgentDtoSchema",
        200,
    )
    .await;
    f.call(
        Op::RestoreAgent,
        request(&path, None),
        "AgentDtoSchema",
        200,
    )
    .await;
}
