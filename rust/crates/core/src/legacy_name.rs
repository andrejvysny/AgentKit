//! Call-time legacy tool names (T5.2, R-API-12). A tool that was renamed or
//! retired keeps answering to its old name for sessions and replays that still
//! emit it: the call maps to the replacement, or the model is taught where to
//! go. The old name never joins the tools array, the manifest, the catalog or
//! tool search; it exists only as data the call pipeline reads.
use crate::{CoreError, ToolCall, ToolRegistry};

/// One retired tool name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyName {
    /// The name models may still emit.
    pub old: String,
    /// The tool that now does the same job. Set only when the old arguments
    /// mean the same thing to it (its own aliases then normalise them).
    pub new: Option<String>,
    /// What the model reads when the call cannot be mapped. Names tools with
    /// `{{tool:..}}` placeholders so it shows the session's own spelling.
    pub teach: String,
}

/// The legacy name's outcome for `call`: `None` when the name is not legacy,
/// the renamed call (with the record note) when its replacement is offered,
/// else the teaching failure.
pub(crate) fn resolve(
    registry: &ToolRegistry,
    call: &ToolCall,
    via_meta: bool,
) -> Option<Result<(ToolCall, String), CoreError>> {
    if call.name == crate::UNNAMED_TOOL || registry.get(&call.name).is_some() {
        return None;
    }
    let legacy = registry.legacy_name(&call.name)?;
    if let Some(new) = &legacy.new
        && registry.is_offered(new, via_meta)
    {
        let mut renamed = call.clone();
        renamed.name = new.clone();
        return Some(Ok((renamed, format!("legacy_name:{}->{new}", legacy.old))));
    }
    let message = format!("Tool {} was retired. {}", legacy.old, legacy.teach);
    let message = crate::dialect::render_text(registry.dialect(), &message);
    Some(Err(CoreError::new("tool_renamed", &message)))
}
