//! Cache capability and strategy per provider (CE-07, INV-5).
//!
//! A provider preset declares one [`CacheCapability`]. The matching
//! [`CacheStrategy`] turns the frozen layout and session prefix into
//! [`CacheDirectives`] that a transport applies verbatim, so a transport
//! never decides on its own whether a cache field is allowed.

use agentkit_contracts::AiSessionPrefix;
use serde::{Deserialize, Serialize};

use crate::{PromptLayout, Segment, ToolDefinition, tools_digest};

/// The request field an implicit-prefix provider reads its cache key from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheHint {
    /// Body field `prompt_cache_key` (OpenAI).
    PromptCacheKey,
    /// Body field `session_id` (OpenRouter).
    SessionIdField,
    /// A conversation header; the transport owns the header name.
    ConversationHeader,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheTtl {
    FiveMinutes,
    OneHour,
}

/// What a provider can do for prompt caching.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CacheCapability {
    /// No caching, or none we can steer. The unknown-provider default.
    #[default]
    None,
    /// The provider caches the longest stable prefix by itself; a hint only
    /// improves routing.
    ImplicitPrefix {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hint: Option<CacheHint>,
    },
    /// The caller marks breakpoints (Anthropic style).
    ExplicitBreakpoints {
        max_breakpoints: u8,
        ttls: Vec<CacheTtl>,
        min_tokens: u64,
    },
    /// A local server keeps a KV slot; `pinning` asks it to hold the slot.
    LocalSlot { pinning: bool },
}

/// Where a breakpoint sits, in request order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BreakpointAt {
    /// BP1: the last tool definition (end of S1).
    LastTool,
    /// BP2: the end of a system segment; the planner uses S4, or the last
    /// system segment that exists.
    EndOfSegment(Segment),
    /// BP3 and BP4: after the message at this index of the request history.
    AfterMessage(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Breakpoint {
    pub at: BreakpointAt,
    pub ttl: CacheTtl,
}

/// What the strategy decided for one request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CacheDirectives {
    /// Cache key to send, with the field that carries it. Both are set or
    /// both are absent.
    pub cache_key: Option<String>,
    pub key_field: Option<CacheHint>,
    /// Explicit breakpoints in request order.
    pub breakpoints: Vec<Breakpoint>,
    /// Local slot should be pinned (`LocalSlot { pinning: true }`).
    pub pin_slot: bool,
}

impl CacheDirectives {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Everything the strategy needs about one request.
#[derive(Clone, Copy, Debug)]
pub struct CacheRequest<'a> {
    pub layout: &'a PromptLayout,
    pub prefix: &'a AiSessionPrefix,
    /// The S1 tool definitions the request carries; `None` when unknown, and
    /// then only the prefix digest says whether tools exist.
    pub tools: Option<&'a [ToolDefinition]>,
    /// Index of the last message the previous request carried (BP3).
    pub previous_end: Option<usize>,
    /// Index of the last message of this request (BP4).
    pub request_end: Option<usize>,
    /// TTL of BP1 and BP2; the profile's setting, one hour by default.
    pub long_ttl: CacheTtl,
}

impl<'a> CacheRequest<'a> {
    /// The static facts only: no tools list, no message positions, 1h.
    pub fn of_prefix(layout: &'a PromptLayout, prefix: &'a AiSessionPrefix) -> Self {
        Self {
            layout,
            prefix,
            tools: None,
            previous_end: None,
            request_end: None,
            long_ttl: CacheTtl::OneHour,
        }
    }
}

pub trait CacheStrategy {
    /// The plan from the static facts alone; see [`CacheRequest::of_prefix`].
    fn plan(&self, layout: &PromptLayout, prefix: &AiSessionPrefix) -> CacheDirectives {
        self.plan_request(&CacheRequest::of_prefix(layout, prefix))
    }
    fn plan_request(&self, request: &CacheRequest<'_>) -> CacheDirectives;
}

impl CacheStrategy for CacheCapability {
    fn plan_request(&self, request: &CacheRequest<'_>) -> CacheDirectives {
        let prefix = request.prefix;
        match self {
            CacheCapability::None => CacheDirectives::default(),
            CacheCapability::ImplicitPrefix { hint } => CacheDirectives {
                cache_key: hint.map(|_| prefix.cache_key.clone()),
                key_field: *hint,
                ..Default::default()
            },
            CacheCapability::LocalSlot { pinning } => CacheDirectives {
                pin_slot: *pinning,
                ..Default::default()
            },
            CacheCapability::ExplicitBreakpoints { .. } => CacheDirectives {
                breakpoints: plan_breakpoints(
                    self,
                    &BreakpointShape::of_request(request),
                    request.long_ttl,
                ),
                ..Default::default()
            },
        }
    }
}

/// The request facts the explicit planner needs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BreakpointShape {
    pub has_tools: bool,
    /// The last system segment present in the layout (S2-S4).
    pub last_system: Option<Segment>,
    /// Estimated tokens of S1-S4, checked against `min_tokens`.
    pub prefix_tokens: u64,
    /// Index of the last message the previous request carried.
    pub previous_end: Option<usize>,
    /// Index of the last message of this request.
    pub request_end: Option<usize>,
}

impl BreakpointShape {
    /// The request's shape. Token estimate is 4 bytes per token over S1
    /// (the serialized tool definitions, when known) and S2-S4 (the system
    /// text).
    pub fn of_request(request: &CacheRequest<'_>) -> Self {
        let system: usize = request
            .layout
            .blocks()
            .iter()
            .map(|block| block.text.len())
            .sum();
        let tools = request.tools.map_or(0, |tools| {
            serde_json::to_string(tools).map_or(0, |text| text.len())
        });
        Self {
            has_tools: request.tools.map_or_else(
                || request.prefix.tools_digest != tools_digest(&[]),
                |tools| !tools.is_empty(),
            ),
            last_system: request.layout.blocks().last().map(|block| block.segment),
            prefix_tokens: (system + tools).div_ceil(4) as u64,
            previous_end: request.previous_end,
            request_end: request.request_end,
        }
    }
}

/// Place breakpoints: BP1 last tool and BP2 end of S4 take `long`
/// (profile-configurable, 1h by default); BP3 end of the previous request and
/// BP4 end of this request take 5m. A TTL the provider does not list falls
/// back to 5m. A prefix under `min_tokens` gets no BP1/BP2. When over
/// `max_breakpoints`, BP3 goes first, then BP1; BP4 and BP2 stay. Output is
/// in request order, never two on the same site.
pub fn plan_breakpoints(
    capability: &CacheCapability,
    shape: &BreakpointShape,
    long: CacheTtl,
) -> Vec<Breakpoint> {
    let CacheCapability::ExplicitBreakpoints {
        max_breakpoints,
        ttls,
        min_tokens,
    } = capability
    else {
        return vec![];
    };
    let long = if ttls.contains(&long) {
        long
    } else {
        CacheTtl::FiveMinutes
    };
    let prefix_ok = shape.prefix_tokens >= *min_tokens;
    // Candidates by drop-last priority: BP4, BP2, BP1, BP3.
    let mut ranked: Vec<(u8, Breakpoint)> = vec![];
    if let Some(end) = shape.request_end {
        ranked.push((4, bp(BreakpointAt::AfterMessage(end), CacheTtl::FiveMinutes)));
    }
    if prefix_ok && let Some(segment) = shape.last_system {
        ranked.push((2, bp(BreakpointAt::EndOfSegment(segment), long)));
    }
    if prefix_ok && shape.has_tools {
        ranked.push((1, bp(BreakpointAt::LastTool, long)));
    }
    if let Some(end) = shape.previous_end
        && shape.request_end != Some(end)
    {
        ranked.push((3, bp(BreakpointAt::AfterMessage(end), CacheTtl::FiveMinutes)));
    }
    ranked.truncate(usize::from(*max_breakpoints));
    ranked.sort_by_key(|(number, _)| match number {
        // Request order: tools, system, previous end, this end.
        1 => 0,
        2 => 1,
        3 => 2,
        _ => 3,
    });
    ranked.into_iter().map(|(_, point)| point).collect()
}

fn bp(at: BreakpointAt, ttl: CacheTtl) -> Breakpoint {
    Breakpoint { at, ttl }
}
