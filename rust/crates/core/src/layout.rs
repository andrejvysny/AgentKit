//! The cache-stable prompt layout (CE-01, ADR 0028).
//!
//! A request is built in six segments, most stable first:
//! S1 tools, S2 system, S3 project rules, S4 core memory, S5 history, S6 the
//! new turn. S1-S4 form the frozen head and change only on a re-freeze. This
//! module owns the system segments (S2-S4) as ordered blocks that every
//! transport sends separately, and the pure freeze rule for [`AiSessionPrefix`].

use agentkit_contracts::{AiSessionPrefix, Optional};
use agentkit_kernel::{canonical_sha256, sha256_hex};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

use crate::{ChatMessage, ToolDefinition};

/// Message metadata key naming the segment a leading system message carries.
pub const SEGMENT_METADATA_KEY: &str = "agentkit.segment";

/// One of the six prompt segments, in wire order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Segment {
    Tools,
    System,
    Rules,
    Memory,
    History,
    Turn,
}

impl Segment {
    pub const ALL: [Segment; 6] = [
        Segment::Tools,
        Segment::System,
        Segment::Rules,
        Segment::Memory,
        Segment::History,
        Segment::Turn,
    ];

    /// The short name used in metadata, events and fixtures: `S1`..`S6`.
    pub fn code(self) -> &'static str {
        match self {
            Segment::Tools => "S1",
            Segment::System => "S2",
            Segment::Rules => "S3",
            Segment::Memory => "S4",
            Segment::History => "S5",
            Segment::Turn => "S6",
        }
    }

    pub fn parse(code: &str) -> Option<Segment> {
        Segment::ALL
            .into_iter()
            .find(|segment| segment.code() == code)
    }

    /// S2-S4 travel as system blocks.
    pub fn is_system(self) -> bool {
        matches!(self, Segment::System | Segment::Rules | Segment::Memory)
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    #[error("segment {0} is not a system segment")]
    NotSystem(&'static str),
    #[error("segment {segment} must come after {after}")]
    OutOfOrder {
        segment: &'static str,
        after: &'static str,
    },
}

/// One system segment's text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemBlock {
    pub segment: Segment,
    pub text: String,
}

/// The system segments S2-S4 of one request, in segment order. Empty
/// segments are omitted, never sent as empty blocks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PromptLayout {
    blocks: Vec<SystemBlock>,
}

impl PromptLayout {
    pub fn new() -> Self {
        Self::default()
    }

    /// A layout holding only S2, the shape callers had before segments.
    pub fn from_system(text: Option<&str>) -> Self {
        let mut layout = Self::new();
        if let Some(text) = text {
            layout
                .push(Segment::System, text)
                .expect("S2 is a system segment");
        }
        layout
    }

    /// The system segments of one request: S2 policy, S3 project rules and
    /// S4 core memory. Empty or absent segments are omitted.
    pub fn of(system: Option<&str>, rules: Option<&str>, memory: Option<&str>) -> Self {
        let mut layout = Self::new();
        for (segment, text) in [
            (Segment::System, system),
            (Segment::Rules, rules),
            (Segment::Memory, memory),
        ] {
            layout
                .push(segment, text.unwrap_or_default())
                .expect("S2-S4 are system segments pushed in order");
        }
        layout
    }

    /// Append a segment. Segments must arrive in strictly increasing order.
    pub fn push(&mut self, segment: Segment, text: impl Into<String>) -> Result<(), LayoutError> {
        if !segment.is_system() {
            return Err(LayoutError::NotSystem(segment.code()));
        }
        if let Some(last) = self.blocks.last()
            && last.segment >= segment
        {
            return Err(LayoutError::OutOfOrder {
                segment: segment.code(),
                after: last.segment.code(),
            });
        }
        let text = text.into();
        if !text.is_empty() {
            self.blocks.push(SystemBlock { segment, text });
        }
        Ok(())
    }

    pub fn blocks(&self) -> &[SystemBlock] {
        &self.blocks
    }

    pub fn text(&self, segment: Segment) -> Option<&str> {
        self.blocks
            .iter()
            .find(|block| block.segment == segment)
            .map(|block| block.text.as_str())
    }

    /// SHA-256 hex of the segment text; an absent segment hashes as empty.
    pub fn digest(&self, segment: Segment) -> String {
        sha256_hex(self.text(segment).unwrap_or_default())
    }

    /// The blocks joined by a blank line, for hosts that need one string.
    pub fn joined(&self) -> Option<String> {
        if self.blocks.is_empty() {
            return None;
        }
        Some(
            self.blocks
                .iter()
                .map(|block| block.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n"),
        )
    }

    /// One `system` message per block, tagged with its segment.
    pub fn to_messages(&self) -> Vec<ChatMessage> {
        self.blocks
            .iter()
            .map(|block| {
                let mut message = ChatMessage::text("system", &block.text);
                let mut metadata = Map::new();
                metadata.insert(SEGMENT_METADATA_KEY.into(), block.segment.code().into());
                message.metadata = Some(metadata);
                message
            })
            .collect()
    }

    /// The blocks as provider text blocks, for transports whose system field
    /// is a block list. Cache directives are the provider's to add.
    pub fn text_blocks(&self) -> Vec<Value> {
        self.blocks
            .iter()
            .map(|block| json!({"type":"text","text":block.text}))
            .collect()
    }
}

/// The segment a message was tagged with, when it is a tagged system message.
pub fn segment_of(message: &ChatMessage) -> Option<Segment> {
    if message.role != "system" {
        return None;
    }
    message
        .metadata
        .as_ref()?
        .get(SEGMENT_METADATA_KEY)?
        .as_str()
        .and_then(Segment::parse)
}

/// Part type of the persisted turn context (roadmap 1.2, ADR 0036).
pub const TURN_CONTEXT_PART_TYPE: &str = "turn_context";
/// Every turn-context text opens with this tag and closes with its pair.
pub const TURN_CONTEXT_OPEN: &str = "<turn_context>";
pub const TURN_CONTEXT_CLOSE: &str = "</turn_context>";

/// Wrapper tags a body must not carry, or it could close its own wrapper or
/// pass for a host block.
const TURN_CONTEXT_TAGS: [&str; 7] = [
    "turn_context",
    "skill",
    "template",
    "source",
    "note",
    "date",
    "context",
];

/// A `turn_context` content part holding `text`.
pub fn turn_context_part(text: &str) -> Value {
    json!({"type": TURN_CONTEXT_PART_TYPE, "text": text})
}

/// Whether `text` is the text of a turn-context part.
pub fn is_turn_context_text(text: &str) -> bool {
    text.starts_with(TURN_CONTEXT_OPEN)
}

/// Whether `after_lt` (the text after a `<`) starts one of `tags`, as an
/// opening or closing tag: any letter case, whitespace allowed around the
/// slash and before the closing `>`.
fn starts_tag(after_lt: &str, tags: &[&str]) -> bool {
    let rest = after_lt.trim_start();
    let rest = rest.strip_prefix('/').unwrap_or(rest).trim_start();
    tags.iter().any(|tag| {
        rest.get(..tag.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(tag))
            && !rest[tag.len()..].starts_with(|c: char| c.is_alphanumeric() || c == '_' || c == '-')
    })
}

/// `text` with a backslash after the `<` of every tag in `tags`.
fn defang(text: &str, tags: &[&str]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('<') {
        out.push_str(&rest[..=at]);
        rest = &rest[at + 1..];
        if starts_tag(rest, tags) {
            out.push('\\');
        }
    }
    out.push_str(rest);
    out
}

/// `text` with every turn-context wrapper tag, opening or closing, defanged
/// (`</skill>` becomes `<\/skill>`), so selected content cannot close its own
/// wrapper or open a block of its own.
pub fn escape_turn_context(text: &str) -> String {
    defang(text, &TURN_CONTEXT_TAGS)
}

/// A message body as a provider receives it: each `turn_context` part becomes
/// a text part of the same text. Typed text has any `<turn_context` tag
/// defanged, so it cannot pass for a host block. The same input always gives
/// the same bytes, so what was sent once is sent again byte for byte (INV-2).
pub fn content_for_wire(content: &Value) -> Value {
    let typed = |text: &str| defang(text, &TURN_CONTEXT_TAGS[..1]);
    let parts = match content {
        Value::String(text) => return Value::String(typed(text)),
        Value::Array(parts) => parts,
        other => return other.clone(),
    };
    Value::Array(
        parts
            .iter()
            .map(|part| {
                if part["type"] == TURN_CONTEXT_PART_TYPE {
                    json!({"type": "text", "text": part["text"]})
                } else if part["type"] == "text"
                    && let Some(text) = part["text"].as_str()
                {
                    let mut part = part.clone();
                    part["text"] = Value::String(typed(text));
                    part
                } else {
                    part.clone()
                }
            })
            .collect(),
    )
}

/// Request-local `selected` content (the legacy carrier,
/// `ChatRequest::selected_content`) placed inside the turn's last user
/// message, never ahead of the system segments (CE-01). Nothing is sent when
/// `selected` is empty. The chat executor does not use this: it persists the
/// selection as a turn-context part on the user message instead.
pub fn fold_selected_content(
    messages: &[ChatMessage],
    selected: &[ChatMessage],
) -> Vec<ChatMessage> {
    let mut folded = messages.to_vec();
    if selected.is_empty() {
        return folded;
    }
    let mut text = String::from(TURN_CONTEXT_OPEN);
    for item in selected {
        let body = escape_turn_context(item.content.as_str().unwrap_or_default());
        text.push_str(&format!("\n<context role=\"{}\">\n{body}\n</context>", item.role));
    }
    text.push('\n');
    text.push_str(TURN_CONTEXT_CLOSE);
    let last_user = folded
        .iter_mut()
        .rev()
        .find(|message| message.role == "user" && message.tool_call_id.is_none());
    match last_user {
        Some(message) => match &mut message.content {
            Value::String(existing) => {
                existing.push_str("\n\n");
                existing.push_str(&text);
            }
            Value::Array(parts) => parts.push(json!({"type": "text", "text": text})),
            _ => {}
        },
        None => folded.push(ChatMessage::text("user", &text)),
    }
    folded
}

/// SHA-256 over the canonical JSON of the tool definitions in request order.
/// A session sends the frozen, name-sorted manifest (`manifest.rs`), so that
/// order is canonical.
pub fn tools_digest(tools: &[ToolDefinition]) -> String {
    canonical_sha256(&serde_json::to_value(tools).expect("tool definitions serialize"))
}

/// Digests of everything S1-S4 and the frozen parameters contain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefixDigests {
    pub tools: String,
    pub system: String,
    pub rules: String,
    pub memory_core: String,
    pub params: String,
}

impl PrefixDigests {
    /// `params` is the JSON object of request parameters held fixed for the
    /// session (model, reasoning, sampling).
    pub fn compute(layout: &PromptLayout, tools: &[ToolDefinition], params: &Value) -> Self {
        Self {
            tools: tools_digest(tools),
            system: layout.digest(Segment::System),
            rules: layout.digest(Segment::Rules),
            memory_core: layout.digest(Segment::Memory),
            params: canonical_sha256(params),
        }
    }

    /// Names of the digests that differ from the recorded prefix.
    pub fn changed(&self, prefix: &AiSessionPrefix) -> Vec<&'static str> {
        [
            ("tools", self.tools != prefix.tools_digest),
            ("system", self.system != prefix.system_digest),
            ("rules", self.rules != prefix.rules_digest),
            ("memoryCore", self.memory_core != prefix.memory_core_digest),
            ("params", self.params != prefix.params_digest),
        ]
        .into_iter()
        .filter_map(|(name, differs)| differs.then_some(name))
        .collect()
    }
}

/// What a turn's freeze check decided.
#[derive(Clone, Debug, PartialEq)]
pub enum Freeze {
    /// First turn of the session: the prefix was recorded at revision 1.
    Created(AiSessionPrefix),
    /// The candidate matches the recorded prefix.
    Unchanged(AiSessionPrefix),
    /// The head changed: a new revision replaces the record. This rewrites
    /// already-sent bytes, so the host reports it.
    Refrozen {
        prefix: AiSessionPrefix,
        changed: Vec<&'static str>,
    },
}

impl Freeze {
    pub fn prefix(&self) -> &AiSessionPrefix {
        match self {
            Freeze::Created(prefix)
            | Freeze::Unchanged(prefix)
            | Freeze::Refrozen { prefix, .. } => prefix,
        }
    }
}

/// Decide the session prefix for this turn. Tool-track fields of a recorded
/// prefix survive a re-freeze; their producers set them.
pub fn freeze(
    prior: Option<&AiSessionPrefix>,
    digests: PrefixDigests,
    chat_id: &str,
    branch_id: &str,
    task_id: &str,
) -> Freeze {
    let build = |revision: u64, carried: Option<&AiSessionPrefix>| AiSessionPrefix {
        revision: revision as f64,
        tools_digest: digests.tools.clone(),
        system_digest: digests.system.clone(),
        rules_digest: digests.rules.clone(),
        memory_core_digest: digests.memory_core.clone(),
        params_digest: digests.params.clone(),
        frozen_at_task: task_id.to_owned(),
        cache_key: cache_key(chat_id, branch_id, revision),
        tool_manifest_hash: carried.map_or(Optional::Absent, |p| p.tool_manifest_hash.clone()),
        tool_manifest: carried.map_or(Optional::Absent, |p| p.tool_manifest.clone()),
        deferred_tools: carried.map_or(Optional::Absent, |p| p.deferred_tools.clone()),
        model_profile_id: carried.map_or(Optional::Absent, |p| p.model_profile_id.clone()),
        dialect_version: carried.map_or(Optional::Absent, |p| p.dialect_version.clone()),
        model_profile_digest: carried.map_or(Optional::Absent, |p| p.model_profile_digest.clone()),
        extra: BTreeMap::new(),
    };
    let Some(prior) = prior else {
        return Freeze::Created(build(1, None));
    };
    let changed = digests.changed(prior);
    if changed.is_empty() {
        return Freeze::Unchanged(prior.clone());
    }
    Freeze::Refrozen {
        prefix: build(prior.revision as u64 + 1, Some(prior)),
        changed,
    }
}

/// `base32(sha256(chat_id, branch_id, revision))[..32]`, lowercase, no padding.
/// A re-freeze changes the key because the old cache no longer applies.
pub fn cache_key(chat_id: &str, branch_id: &str, revision: u64) -> String {
    let digest = sha256_hex(format!("{chat_id}\u{1f}{branch_id}\u{1f}{revision}"));
    let bytes: Vec<u8> = (0..20)
        .map(|index| u8::from_str_radix(&digest[index * 2..index * 2 + 2], 16).expect("hex digest"))
        .collect();
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut key = String::with_capacity(32);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            key.push(char::from(ALPHABET[((buffer >> bits) & 31) as usize]));
        }
    }
    key
}
