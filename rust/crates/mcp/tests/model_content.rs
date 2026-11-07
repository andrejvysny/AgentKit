use agentkit_mcp::{ModelContentCapabilities, project_model_content};
use serde_json::json;

#[test]
fn resources_remain_untrusted_and_media_requires_explicit_capabilities() {
    let content = vec![
        json!({"type":"text","text":"plain"}),
        json!({"type":"resource","resource":{"uri":"file:///report","text":"report"}}),
        json!({"type":"resource_link","uri":"https://remote.test/item","name":"Item","description":"Remote"}),
        json!({"type":"image","mimeType":"image/png","data":"AA=="}),
        json!({"type":"audio","mimeType":"audio/wav","data":"AA=="}),
    ];
    let projection = project_model_content(
        "mcp.docs.read",
        "docs",
        "read",
        &content,
        Default::default(),
    );
    assert_eq!(
        projection.text,
        "plain\n[Source: file:///report]\nreport\n[Resource: https://remote.test/item] Remote"
    );
    assert_eq!(projection.sources.len(), 2);
    assert_eq!(projection.sources[0]["metadata"]["trusted"], false);
    assert!(projection.attachments.is_empty());
    assert_eq!(projection.unsupported, ["image", "audio"]);
    let rich = project_model_content(
        "mcp.docs.read",
        "docs",
        "read",
        &content,
        ModelContentCapabilities {
            image: true,
            audio: true,
        },
    );
    assert_eq!(rich.attachments.len(), 2);
    assert_eq!(rich.attachments[0]["source"]["mediaType"], "image/png");
    assert_eq!(rich.attachments[1]["format"], "wav");
    assert!(rich.unsupported.is_empty());
}

#[test]
fn malformed_or_unsupported_media_never_enters_model_content() {
    let content = vec![
        json!({"type":"image","mimeType":"image/!png","data":"AA=="}),
        json!({"type":"image","mimeType":"image/png;token=secret","data":"AA=="}),
        json!({"type":"audio","mimeType":"audio/ogg","data":"AA=="}),
    ];
    let projection = project_model_content(
        "mcp.docs.read",
        "docs",
        "read",
        &content,
        ModelContentCapabilities {
            image: true,
            audio: true,
        },
    );
    assert!(projection.attachments.is_empty());
    assert_eq!(projection.unsupported, ["image", "audio"]);
}
