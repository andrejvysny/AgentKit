//! Host-owned egress gate for the web tools. The host picks, per tool, whether
//! the model may call it freely, only after a human approves, or never.

use agentkit_governance::{
    ApprovalAsk, GovernanceResult, ToolGuard, ToolGuardContext, ToolGuardVerdict,
};
use async_trait::async_trait;
use serde_json::Value;

const WEB_NAMESPACE: &str = "web";
const NET_PREFIX: &str = "net.";
const PREVIEW_MAX_CHARS: usize = 200;

/// What the host allows for one web tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetDecision {
    Allow,
    Ask,
    Deny,
}

impl NetDecision {
    fn severity(self) -> u8 {
        match self {
            Self::Allow => 0,
            Self::Ask => 1,
            Self::Deny => 2,
        }
    }

    fn stricter(self, other: Self) -> Self {
        if other.severity() > self.severity() {
            other
        } else {
            self
        }
    }
}

/// Per-tool egress policy: `search` covers `net.search`, `fetch` covers
/// `net.fetch`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetPolicy {
    pub search: NetDecision,
    pub fetch: NetDecision,
}

impl NetPolicy {
    pub fn allow_all() -> Self {
        Self {
            search: NetDecision::Allow,
            fetch: NetDecision::Allow,
        }
    }

    pub fn deny_all() -> Self {
        Self {
            search: NetDecision::Deny,
            fetch: NetDecision::Deny,
        }
    }

    fn decide(&self, capability: &str) -> NetDecision {
        match capability {
            "net.search" => self.search,
            "net.fetch" => self.fetch,
            _ => self.search.stricter(self.fetch),
        }
    }
}

/// Gates `web` namespace / `net.*` capability tools by a [`NetPolicy`]; every
/// other tool passes untouched.
pub struct WebGuard {
    policy: NetPolicy,
}

impl WebGuard {
    pub fn new(policy: NetPolicy) -> Self {
        Self { policy }
    }

    fn decision(&self, context: &ToolGuardContext) -> Option<NetDecision> {
        let capability = context.tool.capability.as_str();
        if context.namespace != WEB_NAMESPACE && !capability.starts_with(NET_PREFIX) {
            return None;
        }
        Some(self.policy.decide(capability))
    }
}

fn clamp_preview(text: &str) -> String {
    text.chars().take(PREVIEW_MAX_CHARS).collect()
}

const FETCH_URL_PREVIEW_CHARS: usize = 160;
const USERINFO_WARNING: &str = "Warning: link hides its real host. ";

/// Leads with the real host so a `docs.rs@evil.example` link cannot pass as
/// docs.rs; unparsable input falls back to the raw clamp.
fn fetch_preview(raw: &str) -> String {
    let Ok(url) = url::Url::parse(raw) else {
        return clamp_preview(raw);
    };
    let Some(host) = url.host_str() else {
        return clamp_preview(raw);
    };
    let warning = if url.username().is_empty() && url.password().is_none() {
        ""
    } else {
        USERINFO_WARNING
    };
    let shown: String = raw.chars().take(FETCH_URL_PREVIEW_CHARS).collect();
    format!("{warning}{host} \u{2014} {shown}")
}

fn ask_for(context: &ToolGuardContext) -> ApprovalAsk {
    let arguments = context.arguments.as_ref();
    let text = |key: &str| {
        arguments
            .and_then(|value| value.get(key))
            .and_then(Value::as_str)
            .map(clamp_preview)
    };
    let (noun, preview) = if context.tool.capability == "net.fetch" {
        let raw = arguments
            .and_then(|value| value.get("url"))
            .and_then(Value::as_str);
        ("the URL", raw.map(fetch_preview))
    } else if context.tool.capability == "net.search" {
        ("the query", text("query"))
    } else {
        ("the request", text("query").or_else(|| text("url")))
    };
    ApprovalAsk {
        reason: format!("approval_required: this sends {noun} to an external service"),
        preview,
        ..Default::default()
    }
}

#[async_trait]
impl ToolGuard for WebGuard {
    async fn is_visible(&self, context: &ToolGuardContext) -> GovernanceResult<bool> {
        Ok(self.decision(context) != Some(NetDecision::Deny))
    }

    async fn can_execute(&self, context: &ToolGuardContext) -> GovernanceResult<ToolGuardVerdict> {
        Ok(match self.decision(context) {
            None | Some(NetDecision::Allow) => ToolGuardVerdict::Allowed,
            Some(NetDecision::Deny) => ToolGuardVerdict::Refused(
                "web_disabled: Web access is turned off for this space.".to_owned(),
            ),
            Some(NetDecision::Ask) => ToolGuardVerdict::AwaitingHuman(ask_for(context)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_core::{ToolDefinition, ToolLimits};
    use agentkit_governance::ToolContributionContext;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    fn context(namespace: &str, capability: &str, arguments: Option<Value>) -> ToolGuardContext {
        ToolGuardContext {
            contribution: ToolContributionContext {
                chat_id: None,
                run_id: "run".into(),
                scope_id: None,
                actor: json!({}),
                actor_id: None,
                bindings: vec![],
                limits: ToolLimits::default(),
                cancellation: CancellationToken::new(),
                has_primary_binding: false,
                timeout_ms: 20,
                durable: None,
            },
            namespace: namespace.into(),
            tool: ToolDefinition {
                name: "tool".into(),
                version: "1".into(),
                effect: "read".into(),
                capability: capability.into(),
                description: String::new(),
                input_schema: json!({"type": "object"}),
                output_schema: None,
                timeout_ms: None,
                examples: vec![],
            },
            arguments,
        }
    }

    fn policy(search: NetDecision, fetch: NetDecision) -> WebGuard {
        WebGuard::new(NetPolicy { search, fetch })
    }

    #[tokio::test]
    async fn allow_all_runs_and_shows_web_tools() {
        let guard = WebGuard::new(NetPolicy::allow_all());
        let ctx = context("web", "net.search", Some(json!({"query": "q"})));
        assert!(guard.is_visible(&ctx).await.unwrap());
        assert!(matches!(
            guard.can_execute(&ctx).await.unwrap(),
            ToolGuardVerdict::Allowed
        ));
    }

    #[tokio::test]
    async fn deny_hides_and_refuses() {
        let guard = WebGuard::new(NetPolicy::deny_all());
        let ctx = context("web", "net.fetch", Some(json!({"url": "https://a.test"})));
        assert!(!guard.is_visible(&ctx).await.unwrap());
        match guard.can_execute(&ctx).await.unwrap() {
            ToolGuardVerdict::Refused(text) => {
                assert_eq!(
                    text,
                    "web_disabled: Web access is turned off for this space."
                );
            }
            _ => panic!("expected Refused"),
        }
    }

    #[tokio::test]
    async fn ask_search_previews_the_query() {
        let guard = policy(NetDecision::Ask, NetDecision::Allow);
        let ctx = context("web", "net.search", Some(json!({"query": "rust news"})));
        assert!(guard.is_visible(&ctx).await.unwrap());
        match guard.can_execute(&ctx).await.unwrap() {
            ToolGuardVerdict::AwaitingHuman(ask) => {
                assert_eq!(
                    ask.reason,
                    "approval_required: this sends the query to an external service"
                );
                assert_eq!(ask.preview.as_deref(), Some("rust news"));
            }
            _ => panic!("expected AwaitingHuman"),
        }
        // The fetch half of the policy stays open.
        let fetch = context("web", "net.fetch", Some(json!({"url": "https://a.test"})));
        assert!(matches!(
            guard.can_execute(&fetch).await.unwrap(),
            ToolGuardVerdict::Allowed
        ));
    }

    #[tokio::test]
    async fn ask_fetch_previews_the_clamped_url() {
        let guard = policy(NetDecision::Allow, NetDecision::Ask);
        let url = format!("https://a.test/{}", "x".repeat(400));
        let ctx = context("web", "net.fetch", Some(json!({"url": url})));
        match guard.can_execute(&ctx).await.unwrap() {
            ToolGuardVerdict::AwaitingHuman(ask) => {
                assert_eq!(
                    ask.reason,
                    "approval_required: this sends the URL to an external service"
                );
                assert_eq!(ask.preview.map(|p| p.chars().count()), Some(6 + 3 + 160));
            }
            _ => panic!("expected AwaitingHuman"),
        }
    }

    #[tokio::test]
    async fn ask_fetch_flags_userinfo_links() {
        let guard = policy(NetDecision::Allow, NetDecision::Ask);
        let ctx = context(
            "web",
            "net.fetch",
            Some(json!({"url": "https://docs.rs@evil.example/x"})),
        );
        match guard.can_execute(&ctx).await.unwrap() {
            ToolGuardVerdict::AwaitingHuman(ask) => assert_eq!(
                ask.preview.as_deref(),
                Some(
                    "Warning: link hides its real host. evil.example \u{2014} https://docs.rs@evil.example/x"
                )
            ),
            _ => panic!("expected AwaitingHuman"),
        }
        assert_eq!(fetch_preview("not a url"), "not a url");
    }

    #[tokio::test]
    async fn unknown_net_capability_takes_the_stricter_decision() {
        let guard = policy(NetDecision::Allow, NetDecision::Deny);
        let ctx = context("other", "net.crawl", None);
        assert!(!guard.is_visible(&ctx).await.unwrap());
        let guard = policy(NetDecision::Ask, NetDecision::Allow);
        assert!(matches!(
            guard.can_execute(&ctx).await.unwrap(),
            ToolGuardVerdict::AwaitingHuman(_)
        ));
    }

    #[tokio::test]
    async fn other_tools_are_untouched() {
        let guard = WebGuard::new(NetPolicy::deny_all());
        let ctx = context("coding", "fs.read", None);
        assert!(guard.is_visible(&ctx).await.unwrap());
        assert!(matches!(
            guard.can_execute(&ctx).await.unwrap(),
            ToolGuardVerdict::Allowed
        ));
    }
}
