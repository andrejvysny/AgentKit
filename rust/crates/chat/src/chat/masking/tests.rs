use super::*;
use agentkit_core::ToolCall;

fn plain(id: &str, role: &str, text: &str) -> Entry {
    (ChatMessage::text(role, text), Some(id.into()))
}

fn call(id: &str, name: &str) -> Entry {
    let mut message = ChatMessage::text("assistant", "");
    message.tool_calls.push(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments_json: "{}".into(),
    });
    (message, Some(format!("a-{id}")))
}

fn result(id: &str, bytes: usize) -> Entry {
    let mut message = ChatMessage::text("tool", &"x".repeat(bytes));
    message.tool_call_id = Some(id.into());
    (message, Some(format!("t-{id}")))
}

/// `turns` turns of user, call, 4000-byte result (about 1000 tokens), answer.
/// Turn `t` holds its user at `4t` and its result at `4t + 2`.
fn session(turns: usize, tool: impl Fn(usize) -> &'static str) -> Vec<Entry> {
    let mut entries = vec![];
    for turn in 0..turns {
        let id = format!("c{turn}");
        entries.push(plain(&format!("u{turn}"), "user", &format!("q{turn}")));
        entries.push(call(&id, tool(turn)));
        entries.push(result(&id, 4000));
        entries.push(plain(&format!("d{turn}"), "assistant", "done"));
    }
    entries
}

fn indexes(found: &[Candidate]) -> Vec<usize> {
    found.iter().map(|candidate| candidate.index).collect()
}

#[test]
fn defaults_are_the_spec_and_a_profile_overrides_them() {
    let defaults = MaskSettings::resolve(None);
    assert_eq!(
        (
            defaults.trigger_ratio,
            defaults.target_ratio,
            defaults.min_gain_ratio,
            defaults.keep_recent_turns,
            defaults.image_max_age_turns
        ),
        (0.80, 0.50, 0.20, 3, 6)
    );
    assert!(defaults.enabled);
    let custom = MaskSettings::resolve(Some(&json!({"triggerRatio":0.9,"keepRecentTurns":5})));
    assert_eq!((custom.trigger_ratio, custom.keep_recent_turns), (0.9, 5));
    // Thresholds are validated once, by the descriptor grammar; here they
    // are taken as given.
    let off = MaskSettings::resolve(Some(&json!({"enabled":false})));
    assert!(!off.enabled);
}

#[test]
fn recent_turns_and_excluded_tools_are_never_candidates() {
    let entries = session(6, |turn| match turn {
        0 => "edit_file",
        1 => "memory_search",
        _ => "read_file",
    });
    let found = candidates(&entries, 0, &MaskSettings::default(), true);
    // Turns 3, 4 and 5 are the last three; 0 and 1 are excluded tools.
    assert_eq!(indexes(&found), vec![10]);
    assert!(candidates(&entries, 0, &MaskSettings::default(), false).is_empty());
    // Nothing at or before a summary's first kept message may change.
    assert!(candidates(&entries, 11, &MaskSettings::default(), true).is_empty());
}

fn deferred(id: &str, inner: &str) -> Entry {
    let mut entry = call(id, "tool_call");
    entry.0.tool_calls[0].arguments_json = json!({"name":inner,"arguments":{}}).to_string();
    entry
}

fn turn_of(id: &str, call: Entry) -> Vec<Entry> {
    vec![
        plain(&format!("u-{id}"), "user", "q"),
        call,
        result(id, 4000),
        plain(&format!("d-{id}"), "assistant", "done"),
    ]
}

#[test]
fn a_deferred_call_is_excluded_by_the_tool_it_runs() {
    let mut entries = vec![];
    entries.extend(turn_of("a", deferred("a", "edit_file")));
    entries.extend(turn_of("b", deferred("b", "memory_save")));
    entries.extend(turn_of("c", deferred("c", "read_file")));
    entries.extend(turn_of("d", deferred("d", "toolset_load")));
    entries.extend(turn_of("e", call("e", "toolset_load")));
    for recent in ["r1", "r2", "r3"] {
        entries.extend(turn_of(recent, call(recent, "read_file")));
    }
    let found = candidates(&entries, 0, &MaskSettings::default(), true);
    // Only the deferred read_file result (turn c, index 10) is a candidate:
    // the others are edit_file, memory_*, and toolset_load, deferred or not.
    assert_eq!(indexes(&found), vec![10]);
    assert_eq!(
        found[0].kind,
        Kind::Tool {
            call_id: "c".into(),
            name: "read_file".into()
        }
    );
}

#[test]
fn a_batch_masks_oldest_first_until_the_target() {
    let entries = session(6, |_| "read_file");
    let settings = MaskSettings {
        keep_recent_turns: 1,
        ..MaskSettings::default()
    };
    // B = 7000: E is about 6030, target 3500, so three results (about 2930
    // tokens) bring it low enough and the fourth is left alone.
    let planned = plan(&entries, 0, &settings, 7000, true);
    assert!(!planned.compact);
    assert_eq!(indexes(&planned.picks), vec![2, 6, 10]);
}

#[test]
fn too_little_to_free_signals_compaction_instead() {
    let entries = session(6, |_| "read_file");
    // The three maskable results free about 2930 tokens, under 0.20 of 20000.
    let planned = plan(&entries, 0, &MaskSettings::default(), 20_000, true);
    assert!(planned.compact);
    assert!(planned.picks.is_empty());
    // Without an output store no result may be masked at all.
    let planned = plan(&entries, 0, &MaskSettings::default(), 7000, false);
    assert!(planned.compact);
}

fn hash(entries: &[Entry], covered: &[usize]) -> String {
    source_hash(entries, covered, &mut Hashes::new()).unwrap()
}

fn apply_one(entries: &mut [Entry], record: &Value) -> bool {
    let positions = index_of(entries);
    apply_record(entries, record, &positions, &mut Hashes::new())
}

fn apply_all(entries: &mut [Entry], records: &[Value]) -> Vec<String> {
    let positions = index_of(entries);
    apply_records(entries, records, &positions, &mut Hashes::new())
}

fn record_for(entries: &[Entry], covered: &[usize]) -> Value {
    let masks: Vec<Value> = covered
        .iter()
        .map(|&index| {
            let (message, id) = &entries[index];
            json!({"messageId":id,"callId":message.tool_call_id,"handle":"out_0123456789abcdef0123456789abcdef","bytes":4000})
        })
        .collect();
    json!({"maskId":"m1","sessionId":"s","coveredCallIds":["c0","c1"],"masks":masks,"images":[],
        "sourceHash":hash(entries, covered),"tokensBefore":1,"tokensAfter":1,"createdAt":"2026-11-15T00:00:00.000Z"})
}

#[test]
fn a_record_masks_the_body_and_keeps_the_tool_pairing() {
    let mut entries = session(2, |_| "read_file");
    let record = record_for(&entries, &[2, 6]);
    assert!(apply_one(&mut entries, &record));
    for index in [2, 6] {
        assert_eq!(
            entries[index].0.content,
            json!(
                "[result masked to save context · 4000 bytes · handle out_0123456789abcdef0123456789abcdef]"
            )
        );
        assert_eq!(entries[index].0.role, "tool");
        assert!(entries[index].0.tool_call_id.is_some());
    }
    // Idempotent over its own output is not required; a second pass over the
    // masked history must simply not apply (the source no longer matches).
    assert!(!apply_one(&mut entries, &record));
}

#[test]
fn a_record_over_changed_history_is_skipped_whole() {
    let mut entries = session(2, |_| "read_file");
    let record = record_for(&entries, &[2, 6]);
    entries[6].0.content = json!("edited");
    let before = entries.clone();
    assert!(!apply_one(&mut entries, &record));
    assert_eq!(entries, before);
    // The caller learns which record was dropped, to report the break.
    assert_eq!(apply_all(&mut entries, &[record.clone()]), vec!["m1"]);
    assert_eq!(entries, before);
    let mut missing = session(1, |_| "read_file");
    assert!(!apply_one(&mut missing, &record));
}

#[test]
fn records_apply_in_creation_order_and_mask_the_same_bytes_every_time() {
    let original = session(4, |_| "read_file");
    let first = record_for(&original, &[2]);
    let mut once = original.clone();
    apply_one(&mut once, &first);
    let second = record_for(&once, &[6]);
    // The store lists newest first.
    let records = vec![second, first];
    let mut a = original.clone();
    let mut b = original.clone();
    assert!(apply_all(&mut a, &records).is_empty());
    assert!(apply_all(&mut b, &records).is_empty());
    assert_eq!(a, b);
    // A restart re-assembles the original history and replays the stored
    // records: the provider view is identical.
    let mut restarted = original.clone();
    apply_all(&mut restarted, &records);
    assert_eq!(restarted, a);
    assert!(is_masked(&a[2].0.content) && is_masked(&a[6].0.content));
    assert!(!is_masked(&a[10].0.content));
}

fn image_entry(id: &str, source: Value) -> Entry {
    let mut message = ChatMessage::text("user", "");
    message.content = json!([{"type":"text","text":"look"},{"type":"image","source":source}]);
    (message, Some(id.into()))
}

#[test]
fn images_older_than_six_turns_become_a_placeholder_naming_the_attachment() {
    let mut entries = vec![image_entry("u0", json!({"kind":"ref","ref":"shot.png"}))];
    for turn in 1..6 {
        entries.push(plain(&format!("u{turn}"), "user", "next"));
    }
    // Six turns in all: the image is still inside the last six.
    assert!(candidates(&entries, 0, &MaskSettings::default(), true).is_empty());
    entries.push(plain("u6", "user", "next"));
    let found = candidates(&entries, 0, &MaskSettings::default(), true);
    assert_eq!(indexes(&found), vec![0]);
    assert_eq!(
        found[0].kind,
        Kind::Images {
            names: vec!["shot.png".into()]
        }
    );
    let masked = mask_images(&entries[0].0.content, &["shot.png".to_owned()]).unwrap();
    assert_eq!(
        masked,
        json!([{"type":"text","text":"look"},{"type":"text","text":"[image omitted to save context · attachment shot.png]"}])
    );
    assert!(mask_images(&entries[0].0.content, &[]).is_none());
}

#[test]
fn an_image_placeholder_carries_the_attachment_id_when_it_differs_from_the_name() {
    let content =
        json!([{"type":"image","name":"design.png","source":{"kind":"ref","ref":"att_42"}}]);
    assert_eq!(
        mask_images(&content, &["design.png".to_owned()]).unwrap(),
        json!([{"type":"text","text":"[image omitted to save context · attachment design.png · id att_42]"}])
    );
}

#[test]
fn image_names_come_from_the_source() {
    let url = json!({"type":"image","source":{"kind":"url","url":"https://x.test/a/b.png?sig=1"}});
    let data =
        json!({"type":"image","source":{"kind":"data","base64":"AA==","mediaType":"image/png"}});
    let named = json!({"type":"image","name":"design.png","source":{"kind":"ref","ref":"r"}});
    assert_eq!(image_name(&url), "b.png");
    assert_eq!(image_name(&data), "inline image/png");
    assert_eq!(image_name(&named), "design.png");
}

#[test]
fn a_preview_footer_yields_its_handle() {
    let text = "head\n[output stored: 9,000 bytes, 3 lines · handle out_0123456789abcdef0123456789abcdef\n read_output(handle, start_line, max_lines)]";
    assert_eq!(
        find_handle(text).unwrap().as_str(),
        "out_0123456789abcdef0123456789abcdef"
    );
    assert!(find_handle("handle out_short").is_none());
    assert!(find_handle("nothing here").is_none());
}
