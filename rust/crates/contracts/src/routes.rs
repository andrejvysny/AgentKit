// Generated from packages/contracts/src via codegen/generate.mjs.
use serde::{Deserialize, Serialize};

pub const CONTRACT_VERSION: &str = "0.7.0";
pub const REST_API_VERSION: &str = "v1";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum RestMethod {
    Get,
    Post,
    Patch,
    Delete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RestOperation {
    CreateChat,
    ListChats,
    GetChat,
    UpdateChat,
    DeleteChat,
    ListMessages,
    SubmitMessage,
    RegenerateMessage,
    ForkChat,
    SearchMessages,
    ActivateBranch,
    ListSiblings,
    GetRun,
    StreamRun,
    CancelRun,
    ResumeRun,
    ListToolEvents,
    ListProposals,
    ApproveProposal,
    RejectProposal,
    ApplyProposal,
    ListProviders,
    CreateProvider,
    UpdateProvider,
    DeleteProvider,
    ListModels,
    RefreshProviderModels,
    TestProvider,
    GetSettings,
    UpdateSettings,
    ListAllowances,
    GrantAllowance,
    RevokeAllowance,
    ListConnections,
    SaveConnection,
    DeleteConnection,
    ConnectionAction,
    ListPromptTemplates,
    PublishPromptTemplate,
    DeletePromptTemplate,
    RenderPromptTemplate,
    ListSkills,
    ListMcpServers,
    CreateMcpServer,
    UpdateMcpServer,
    DeleteMcpServer,
    ListTools,
    GetVersion,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RouteDef {
    pub operation: RestOperation,
    pub method: RestMethod,
    pub path: &'static str,
    pub request_schema: Option<&'static str>,
}

pub static REST_ROUTES: &[RouteDef] = &[
    RouteDef {
        operation: RestOperation::CreateChat,
        method: RestMethod::Post,
        path: "/v1/chats",
        request_schema: Some("CreateChatRequestSchema"),
    },
    RouteDef {
        operation: RestOperation::ListChats,
        method: RestMethod::Get,
        path: "/v1/chats",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::GetChat,
        method: RestMethod::Get,
        path: "/v1/chats/:chatId",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::UpdateChat,
        method: RestMethod::Patch,
        path: "/v1/chats/:chatId",
        request_schema: Some("UpdateChatRequestSchema"),
    },
    RouteDef {
        operation: RestOperation::DeleteChat,
        method: RestMethod::Delete,
        path: "/v1/chats/:chatId",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::ListMessages,
        method: RestMethod::Get,
        path: "/v1/chats/:chatId/messages",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::SubmitMessage,
        method: RestMethod::Post,
        path: "/v1/chats/:chatId/messages",
        request_schema: Some("SubmitMessageRequestSchema"),
    },
    RouteDef {
        operation: RestOperation::RegenerateMessage,
        method: RestMethod::Post,
        path: "/v1/chats/:chatId/messages/:messageId/regenerate",
        request_schema: Some("RegenerateMessageRequestSchema"),
    },
    RouteDef {
        operation: RestOperation::ForkChat,
        method: RestMethod::Post,
        path: "/v1/chats/:chatId/fork",
        request_schema: Some("ForkChatRequestSchema"),
    },
    RouteDef {
        operation: RestOperation::SearchMessages,
        method: RestMethod::Get,
        path: "/v1/search",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::ActivateBranch,
        method: RestMethod::Post,
        path: "/v1/messages/:messageId/activate",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::ListSiblings,
        method: RestMethod::Get,
        path: "/v1/messages/:messageId/siblings",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::GetRun,
        method: RestMethod::Get,
        path: "/v1/runs/:runId",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::StreamRun,
        method: RestMethod::Get,
        path: "/v1/runs/:runId/stream",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::CancelRun,
        method: RestMethod::Post,
        path: "/v1/runs/:runId/cancel",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::ResumeRun,
        method: RestMethod::Post,
        path: "/v1/runs/:runId/resume",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::ListToolEvents,
        method: RestMethod::Get,
        path: "/v1/chats/:chatId/tool-events",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::ListProposals,
        method: RestMethod::Get,
        path: "/v1/chats/:chatId/proposals",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::ApproveProposal,
        method: RestMethod::Post,
        path: "/v1/proposals/:proposalId/approve",
        request_schema: Some("ProposalDecisionRequestSchema"),
    },
    RouteDef {
        operation: RestOperation::RejectProposal,
        method: RestMethod::Post,
        path: "/v1/proposals/:proposalId/reject",
        request_schema: Some("ProposalDecisionRequestSchema"),
    },
    RouteDef {
        operation: RestOperation::ApplyProposal,
        method: RestMethod::Post,
        path: "/v1/proposals/:proposalId/apply",
        request_schema: Some("ApplyProposalRequestSchema"),
    },
    RouteDef {
        operation: RestOperation::ListProviders,
        method: RestMethod::Get,
        path: "/v1/providers",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::CreateProvider,
        method: RestMethod::Post,
        path: "/v1/providers",
        request_schema: Some("CreateProviderRequestSchema"),
    },
    RouteDef {
        operation: RestOperation::UpdateProvider,
        method: RestMethod::Patch,
        path: "/v1/providers/:providerId",
        request_schema: Some("UpdateProviderRequestSchema"),
    },
    RouteDef {
        operation: RestOperation::DeleteProvider,
        method: RestMethod::Delete,
        path: "/v1/providers/:providerId",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::ListModels,
        method: RestMethod::Get,
        path: "/v1/providers/:providerId/models",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::RefreshProviderModels,
        method: RestMethod::Post,
        path: "/v1/providers/:providerId/models/refresh",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::TestProvider,
        method: RestMethod::Post,
        path: "/v1/providers/:providerId/test",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::GetSettings,
        method: RestMethod::Get,
        path: "/v1/settings",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::UpdateSettings,
        method: RestMethod::Patch,
        path: "/v1/settings",
        request_schema: Some("UpdateSettingsRequestSchema"),
    },
    RouteDef {
        operation: RestOperation::ListAllowances,
        method: RestMethod::Get,
        path: "/v1/chats/:chatId/write-policy/allowances",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::GrantAllowance,
        method: RestMethod::Post,
        path: "/v1/chats/:chatId/write-policy/allowances",
        request_schema: Some("GrantAllowanceRequestSchema"),
    },
    RouteDef {
        operation: RestOperation::RevokeAllowance,
        method: RestMethod::Delete,
        path: "/v1/chats/:chatId/write-policy/allowances/:allowanceId",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::ListConnections,
        method: RestMethod::Get,
        path: "/v1/connections",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::SaveConnection,
        method: RestMethod::Patch,
        path: "/v1/connections/:connectionId",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::DeleteConnection,
        method: RestMethod::Delete,
        path: "/v1/connections/:connectionId",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::ConnectionAction,
        method: RestMethod::Post,
        path: "/v1/connections/:connectionId/actions",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::ListPromptTemplates,
        method: RestMethod::Get,
        path: "/v1/prompt-templates",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::PublishPromptTemplate,
        method: RestMethod::Post,
        path: "/v1/prompt-templates",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::DeletePromptTemplate,
        method: RestMethod::Delete,
        path: "/v1/prompt-templates/:templateId/:revision",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::RenderPromptTemplate,
        method: RestMethod::Post,
        path: "/v1/prompt-templates/:templateId/:revision/render",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::ListSkills,
        method: RestMethod::Get,
        path: "/v1/skills",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::ListMcpServers,
        method: RestMethod::Get,
        path: "/v1/mcp/servers",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::CreateMcpServer,
        method: RestMethod::Post,
        path: "/v1/mcp/servers",
        request_schema: Some("CreateMcpServerRequestSchema"),
    },
    RouteDef {
        operation: RestOperation::UpdateMcpServer,
        method: RestMethod::Patch,
        path: "/v1/mcp/servers/:serverId",
        request_schema: Some("UpdateMcpServerRequestSchema"),
    },
    RouteDef {
        operation: RestOperation::DeleteMcpServer,
        method: RestMethod::Delete,
        path: "/v1/mcp/servers/:serverId",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::ListTools,
        method: RestMethod::Get,
        path: "/v1/tools",
        request_schema: None,
    },
    RouteDef {
        operation: RestOperation::GetVersion,
        method: RestMethod::Get,
        path: "/v1/version",
        request_schema: None,
    },
];

impl RestOperation {
    pub fn route(&self) -> &'static RouteDef {
        REST_ROUTES
            .iter()
            .find(|route| &route.operation == self)
            .expect("all operations have a route")
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::CreateChat => "createChat",
            Self::ListChats => "listChats",
            Self::GetChat => "getChat",
            Self::UpdateChat => "updateChat",
            Self::DeleteChat => "deleteChat",
            Self::ListMessages => "listMessages",
            Self::SubmitMessage => "submitMessage",
            Self::RegenerateMessage => "regenerateMessage",
            Self::ForkChat => "forkChat",
            Self::SearchMessages => "searchMessages",
            Self::ActivateBranch => "activateBranch",
            Self::ListSiblings => "listSiblings",
            Self::GetRun => "getRun",
            Self::StreamRun => "streamRun",
            Self::CancelRun => "cancelRun",
            Self::ResumeRun => "resumeRun",
            Self::ListToolEvents => "listToolEvents",
            Self::ListProposals => "listProposals",
            Self::ApproveProposal => "approveProposal",
            Self::RejectProposal => "rejectProposal",
            Self::ApplyProposal => "applyProposal",
            Self::ListProviders => "listProviders",
            Self::CreateProvider => "createProvider",
            Self::UpdateProvider => "updateProvider",
            Self::DeleteProvider => "deleteProvider",
            Self::ListModels => "listModels",
            Self::RefreshProviderModels => "refreshProviderModels",
            Self::TestProvider => "testProvider",
            Self::GetSettings => "getSettings",
            Self::UpdateSettings => "updateSettings",
            Self::ListAllowances => "listAllowances",
            Self::GrantAllowance => "grantAllowance",
            Self::RevokeAllowance => "revokeAllowance",
            Self::ListConnections => "listConnections",
            Self::SaveConnection => "saveConnection",
            Self::DeleteConnection => "deleteConnection",
            Self::ConnectionAction => "connectionAction",
            Self::ListPromptTemplates => "listPromptTemplates",
            Self::PublishPromptTemplate => "publishPromptTemplate",
            Self::DeletePromptTemplate => "deletePromptTemplate",
            Self::RenderPromptTemplate => "renderPromptTemplate",
            Self::ListSkills => "listSkills",
            Self::ListMcpServers => "listMcpServers",
            Self::CreateMcpServer => "createMcpServer",
            Self::UpdateMcpServer => "updateMcpServer",
            Self::DeleteMcpServer => "deleteMcpServer",
            Self::ListTools => "listTools",
            Self::GetVersion => "getVersion",
        }
    }
    pub fn request_schema(&self) -> Option<&'static str> {
        self.route().request_schema
    }
}

impl RestMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

impl std::fmt::Display for RestOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RestOperation {
    type Err = serde_json::Error;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        serde_json::from_value(serde_json::Value::String(value.to_owned()))
    }
}
