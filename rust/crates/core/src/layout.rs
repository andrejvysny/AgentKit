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

/// SHA-256 over the canonical JSON of the tool definitions in request order.
/// The sorted manifest of roadmap 1.3 will make that order canonical.
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
        model_profile_id: carried.map_or(Optional::Absent, |p| p.model_profile_id.clone()),
        dialect_version: carried.map_or(Optional::Absent, |p| p.dialect_version.clone()),
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
