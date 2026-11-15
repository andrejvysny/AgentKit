use agentkit_contracts::Optional;
use agentkit_core::{
    ChatMessage, Freeze, LayoutError, PrefixDigests, PromptLayout, SEGMENT_METADATA_KEY, Segment,
    ToolDefinition, cache_key, freeze, segment_of, tools_digest,
};
use agentkit_kernel::sha256_hex;
use serde_json::json;

fn tool(name: &str) -> ToolDefinition {
    serde_json::from_value(json!({"name":name,"version":"1","effect":"read","capability":"c","description":"d","inputSchema":{"type":"object"}})).unwrap()
}
fn digests(layout: &PromptLayout) -> PrefixDigests {
    PrefixDigests::compute(layout, &[tool("a")], &json!({"model":"m"}))
}

#[test]
fn segments_have_stable_codes_in_wire_order() {
    let codes: Vec<_> = Segment::ALL.iter().map(|segment| segment.code()).collect();
    assert_eq!(codes, ["S1", "S2", "S3", "S4", "S5", "S6"]);
    assert!(Segment::ALL.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(Segment::parse("S3"), Some(Segment::Rules));
    assert_eq!(Segment::parse("S7"), None);
    assert!(Segment::System.is_system() && !Segment::History.is_system());
}

#[test]
fn layout_orders_blocks_and_omits_empty_segments() {
    let layout = PromptLayout::of(Some("policy"), None, Some("memory"));
    let blocks: Vec<_> = layout
        .blocks()
        .iter()
        .map(|block| (block.segment, block.text.as_str()))
        .collect();
    assert_eq!(
        blocks,
        [(Segment::System, "policy"), (Segment::Memory, "memory")]
    );
    assert_eq!(layout.joined().as_deref(), Some("policy\n\nmemory"));
    assert_eq!(PromptLayout::of(None, Some(""), None).joined(), None);
    assert_eq!(layout.digest(Segment::Rules), sha256_hex(""));
    assert_eq!(layout.digest(Segment::System), sha256_hex("policy"));
}

#[test]
fn layout_refuses_non_system_and_out_of_order_segments() {
    let mut layout = PromptLayout::new();
    assert_eq!(
        layout.push(Segment::History, "x"),
        Err(LayoutError::NotSystem("S5"))
    );
    layout.push(Segment::Rules, "rules").unwrap();
    assert_eq!(
        layout.push(Segment::System, "late"),
        Err(LayoutError::OutOfOrder {
            segment: "S2",
            after: "S3"
        })
    );
    assert_eq!(
        layout.push(Segment::Rules, "again"),
        Err(LayoutError::OutOfOrder {
            segment: "S3",
            after: "S3"
        })
    );
}

#[test]
fn messages_carry_their_segment_and_text_blocks_follow_order() {
    let layout = PromptLayout::of(Some("policy"), Some("rules"), Some("memory"));
    let messages = layout.to_messages();
    let tagged: Vec<_> = messages
        .iter()
        .map(|message| {
            (
                message.role.as_str(),
                segment_of(message),
                message.content.clone(),
            )
        })
        .collect();
    assert_eq!(
        tagged,
        [
            ("system", Some(Segment::System), json!("policy")),
            ("system", Some(Segment::Rules), json!("rules")),
            ("system", Some(Segment::Memory), json!("memory")),
        ]
    );
    assert_eq!(
        layout.text_blocks(),
        [
            json!({"type":"text","text":"policy"}),
            json!({"type":"text","text":"rules"}),
            json!({"type":"text","text":"memory"}),
        ]
    );
    let mut user = ChatMessage::text("user", "hi");
    assert_eq!(segment_of(&user), None);
    user.metadata = messages[0].metadata.clone();
    assert_eq!(segment_of(&user), None, "only system messages are segments");
    assert!(
        messages[0]
            .metadata
            .as_ref()
            .unwrap()
            .contains_key(SEGMENT_METADATA_KEY)
    );
}

#[test]
fn first_turn_creates_revision_one_and_the_same_head_is_unchanged() {
    let layout = PromptLayout::of(Some("policy"), Some("rules"), None);
    let Freeze::Created(prefix) = freeze(None, digests(&layout), "chat", "", "task-1") else {
        panic!("first turn creates the prefix");
    };
    assert_eq!(prefix.revision, 1.0);
    assert_eq!(prefix.frozen_at_task, "task-1");
    assert_eq!(prefix.system_digest, sha256_hex("policy"));
    assert_eq!(prefix.rules_digest, sha256_hex("rules"));
    assert_eq!(prefix.memory_core_digest, sha256_hex(""));
    assert_eq!(prefix.tools_digest, tools_digest(&[tool("a")]));
    assert_eq!(prefix.cache_key, cache_key("chat", "", 1));
    assert_eq!(prefix.tool_manifest_hash, Optional::Absent);
    let next = freeze(Some(&prefix), digests(&layout), "chat", "", "task-2");
    assert_eq!(next, Freeze::Unchanged(prefix));
}

#[test]
fn a_changed_head_refreezes_under_a_new_revision_and_names_what_changed() {
    let layout = PromptLayout::of(Some("policy"), Some("rules"), None);
    let mut prior = freeze(None, digests(&layout), "chat", "b", "task-1")
        .prefix()
        .clone();
    prior.tool_manifest_hash = Optional::Present("a".repeat(64));
    prior.model_profile_id = Optional::Present("profile".into());
    let edited = PromptLayout::of(Some("policy"), Some("rules edited"), None);
    let Freeze::Refrozen { prefix, changed } =
        freeze(Some(&prior), digests(&edited), "chat", "b", "task-9")
    else {
        panic!("an edited rules block re-freezes");
    };
    assert_eq!(changed, ["rules"]);
    assert_eq!(prefix.revision, 2.0);
    assert_eq!(prefix.frozen_at_task, "task-9");
    assert_ne!(prefix.cache_key, prior.cache_key);
    assert_eq!(prefix.tool_manifest_hash, prior.tool_manifest_hash);
    assert_eq!(prefix.model_profile_id, prior.model_profile_id);
    assert_eq!(prefix.dialect_version, Optional::Absent);
}

#[test]
fn tool_and_parameter_changes_are_detected() {
    let layout = PromptLayout::of(Some("policy"), None, None);
    let prior = freeze(None, digests(&layout), "chat", "", "t")
        .prefix()
        .clone();
    let other_tools =
        PrefixDigests::compute(&layout, &[tool("a"), tool("b")], &json!({"model":"m"}));
    let other_params = PrefixDigests::compute(&layout, &[tool("a")], &json!({"model":"n"}));
    assert_eq!(other_tools.changed(&prior), ["tools"]);
    assert_eq!(other_params.changed(&prior), ["params"]);
    // Parameter key order is canonicalised away.
    let reordered = PrefixDigests::compute(&layout, &[tool("a")], &json!({"model":"m"}));
    assert!(reordered.changed(&prior).is_empty());
}

#[test]
fn cache_key_is_32_base32_chars_stable_and_revision_sensitive() {
    let key = cache_key("chat", "branch", 1);
    assert_eq!(key.len(), 32);
    assert!(
        key.bytes()
            .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
    );
    assert_eq!(key, cache_key("chat", "branch", 1));
    assert_ne!(key, cache_key("chat", "branch", 2));
    assert_ne!(key, cache_key("chat", "other", 1));
    assert_ne!(cache_key("a", "bc", 1), cache_key("ab", "c", 1));
}
