use agentkit_core::{ChatMessage, ToolDefinition, ToolResult};
use serde::de::DeserializeOwned;
use serde_json::Value;

fn decode_both<T: DeserializeOwned>(raw: &str) -> [T; 2] {
    [
        serde_json::from_str(raw).unwrap(),
        serde_json::from_value(agentkit_contracts::parse_javascript_json(raw).unwrap()).unwrap(),
    ]
}
fn assert_marker(value: &Value) {
    assert!(
        value.is_object(),
        "Object must not become a numeric node: {value:?}"
    );
    assert_eq!(value["$serde_json::private::Number"], "123");
}
#[test]
fn chat_message_content_and_metadata_preserve_private_number_keys() {
    let raw = r#"{"role":"user","content":{"$serde_json::private::Number":"123"},"metadata":{"nested":{"$serde_json::private::Number":"123"}}}"#;
    for message in decode_both::<ChatMessage>(raw) {
        assert_marker(&message.content);
        assert_marker(&message.metadata.unwrap()["nested"]);
    }
}
#[test]
fn private_marker_names_with_arbitrary_string_values_remain_user_objects() {
    for (key, value) in [
        ("$serde_json::private::Number", "not a number"),
        ("$serde_json::private::RawValue", "[1]"),
    ] {
        let raw = format!(r#"{{"role":"user","content":{{"{key}":"{value}"}}}}"#);
        for message in decode_both::<ChatMessage>(&raw) {
            assert!(message.content.is_object());
            assert_eq!(message.content[key], value);
        }
    }
}
#[test]
fn tool_schema_defaults_and_constants_preserve_private_number_keys() {
    let raw = r#"{"name":"echo","version":"1","effect":"read","capability":"test","description":"Echo","inputSchema":{"type":"object","default":{"$serde_json::private::Number":"123"}},"outputSchema":{"const":{"$serde_json::private::Number":"123"}}}"#;
    for definition in decode_both::<ToolDefinition>(raw) {
        assert_marker(&definition.input_schema["default"]);
        assert_marker(&definition.output_schema.unwrap()["const"]);
    }
}
#[test]
fn tool_result_data_sources_and_model_data_preserve_private_number_keys() {
    let raw = r#"{"ok":true,"data":{"$serde_json::private::Number":"123"},"sources":[{"$serde_json::private::Number":"123"}],"warnings":[],"truncated":false,"limits":{"profile":"small","maxBytes":16384},"modelData":{"$serde_json::private::Number":"123"}}"#;
    for result in decode_both::<ToolResult>(raw) {
        assert_marker(result.data.as_ref().unwrap());
        assert_marker(&result.sources[0]);
        assert_marker(result.model_data.as_ref().unwrap());
    }
}
