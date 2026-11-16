//! The structured brief that replaces the free-text spawn prompt (roadmap
//! 4.1, delegation spec "Brief contract", DEL-01, DEL-02).
//!
//! A child sees nothing of the parent's conversation, so the brief carries
//! everything it needs: what to produce, where to work, when to stop, and
//! references to context by handle, path, record or message instead of
//! pasted text. The first message the child receives is rendered from the
//! brief by one fixed template per role ([`render_brief`]), so siblings of a
//! role share the same prefix up to their own objective.

use agentkit_core::OutputHandle;
use serde_json::Value;
use std::fmt;
use std::path::{Component, Path, PathBuf};

/// Longest title, in characters (shown in the UI).
pub const TITLE_MAX_CHARS: usize = 80;
/// A `message` reference copies at most this many bytes of the parent's text.
pub const MESSAGE_REF_CAP_BYTES: usize = 4 * 1024;
const OBJECTIVE_MAX_CHARS: usize = 8_000;
const STOP_WHEN_MAX_CHARS: usize = 1_000;
const SCOPE_ITEMS_MAX: usize = 32;
const SCOPE_ITEM_MAX_CHARS: usize = 300;
const REFS_MAX: usize = 16;
const NOTE_MAX_CHARS: usize = 300;
const REF_ID_MAX_CHARS: usize = 128;
const PATH_REF_MAX_CHARS: usize = 1_024;

/// A rejected brief. The text names the field, so the model can repair the
/// call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BriefError(pub String);

impl fmt::Display for BriefError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for BriefError {}

fn invalid(message: impl fmt::Display) -> BriefError {
    BriefError(format!("invalid_brief: {message}"))
}

fn missing(field: &str) -> BriefError {
    invalid(format!("missing required field \"{field}\""))
}

fn wrong(field: &str, expected: &str) -> BriefError {
    invalid(format!("field \"{field}\" must be {expected}"))
}

/// The shape of the child's final message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    Summary,
    Findings,
    Review,
    PatchReport,
}

impl OutputFormat {
    /// Wire names, in schema order.
    pub const NAMES: [&'static str; 4] = ["summary", "findings", "review", "patch_report"];

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "summary" => Some(Self::Summary),
            "findings" => Some(Self::Findings),
            "review" => Some(Self::Review),
            "patch_report" => Some(Self::PatchReport),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Summary => "summary",
            Self::Findings => "findings",
            Self::Review => "review",
            Self::PatchReport => "patch_report",
        }
    }

    /// The fixed instruction the template gives for this format.
    fn instructions(self) -> &'static str {
        match self {
            Self::Summary => {
                "End with a final message that summarizes the result in plain paragraphs, \
                 most important facts first."
            }
            Self::Findings => {
                "End with a final message that lists each finding as a claim with its \
                 supporting evidence (file and location) and your confidence (high, medium or \
                 low). Call report_findings with the same findings before you finish."
            }
            Self::Review => {
                "End with a final message that reviews the work: defects first, each with \
                 file, location and why it matters; then missing tests; then a verdict."
            }
            Self::PatchReport => {
                "End with a final message that reports the change you made: the files \
                 changed, what each change does, and what you verified (tests run and their \
                 result)."
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BriefScope {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub stop_when: String,
}

/// What a context reference points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefKind {
    /// An output-store handle the child may read (per-handle grant).
    Output,
    /// A path inside the inherited workspace.
    Path,
    /// A memory record id (memory tools arrive with roadmap 3.3).
    Memory,
    /// One parent message whose text is copied, capped at 4 KiB.
    Message,
}

impl RefKind {
    pub const NAMES: [&'static str; 4] = ["output", "path", "memory", "message"];

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "output" => Some(Self::Output),
            "path" => Some(Self::Path),
            "memory" => Some(Self::Memory),
            "message" => Some(Self::Message),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Output => "output",
            Self::Path => "path",
            Self::Memory => "memory",
            Self::Message => "message",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextRef {
    pub kind: RefKind,
    pub reference: String,
    pub note: Option<String>,
}

/// Requested limits; the service clamps each to its ceiling.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BriefBudget {
    pub max_tokens: Option<u64>,
    pub max_provider_requests: Option<u64>,
    pub timeout_ms: Option<u64>,
    pub max_iterations: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Brief {
    pub role: String,
    pub title: String,
    pub objective: String,
    pub output_format: OutputFormat,
    pub scope: BriefScope,
    pub context_refs: Vec<ContextRef>,
    pub budget: BriefBudget,
    pub model: Option<String>,
}

impl Brief {
    /// A brief with the required fields and nothing else.
    pub fn new(
        role: &str,
        title: &str,
        objective: &str,
        output_format: OutputFormat,
        stop_when: &str,
    ) -> Self {
        Self {
            role: role.to_owned(),
            title: title.to_owned(),
            objective: objective.to_owned(),
            output_format,
            scope: BriefScope {
                include: vec![],
                exclude: vec![],
                stop_when: stop_when.to_owned(),
            },
            context_refs: vec![],
            budget: BriefBudget::default(),
            model: None,
        }
    }

    /// Digest of every field of the brief, stored on the child at spawn so a
    /// replay of the same spawn key with a different brief is refused
    /// instead of silently returning the first child.
    pub fn digest(&self) -> String {
        let refs: Vec<Value> = self
            .context_refs
            .iter()
            .map(|reference| {
                serde_json::json!([reference.kind.as_str(), reference.reference, reference.note])
            })
            .collect();
        agentkit_kernel::sha256_hex(
            serde_json::json!({
                "role": self.role,
                "title": self.title,
                "objective": self.objective,
                "outputFormat": self.output_format.as_str(),
                "include": self.scope.include,
                "exclude": self.scope.exclude,
                "stopWhen": self.scope.stop_when,
                "refs": refs,
                "maxTokens": self.budget.max_tokens,
                "maxProviderRequests": self.budget.max_provider_requests,
                "timeoutMs": self.budget.timeout_ms,
                "maxIterations": self.budget.max_iterations,
                "model": self.model,
            })
            .to_string(),
        )
    }

    /// Parse the `subagent_spawn` arguments. A required field that is
    /// missing or empty is rejected with its name (`scope.stop_when` for the
    /// completion condition).
    pub fn from_input(input: &Value) -> Result<Self, BriefError> {
        let role = required_text(input, "role", "role")?;
        let title = required_text(input, "title", "title")?;
        let objective = required_text(input, "objective", "objective")?;
        let format_text = required_text(input, "output_format", "output_format")?;
        let output_format = OutputFormat::parse(&format_text).ok_or_else(|| {
            wrong(
                "output_format",
                &format!("one of {}", OutputFormat::NAMES.join(", ")),
            )
        })?;
        let scope_value = match input.get("scope") {
            None | Some(Value::Null) => return Err(missing("scope.stop_when")),
            Some(value) if value.is_object() => value,
            Some(_) => return Err(wrong("scope", "an object")),
        };
        let stop_when = required_text(scope_value, "stop_when", "scope.stop_when")?;
        let scope = BriefScope {
            include: text_list(scope_value.get("include"), "scope.include")?,
            exclude: text_list(scope_value.get("exclude"), "scope.exclude")?,
            stop_when,
        };
        let brief = Self {
            role,
            title,
            objective,
            output_format,
            scope,
            context_refs: parse_refs(input.get("context_refs"))?,
            budget: parse_budget(input.get("budget"))?,
            model: optional_text(input, "model", "model")?,
        };
        brief.validate()?;
        Ok(brief)
    }

    /// The `subagent_spawn` argument form of this brief, the inverse of
    /// [`Brief::from_input`]: a held child (warm-then-fan) keeps it on its
    /// chat so a restarted process can still dispatch it.
    pub fn to_input(&self) -> Value {
        let refs: Vec<Value> = self
            .context_refs
            .iter()
            .map(|reference| {
                serde_json::json!({
                    "kind": reference.kind.as_str(),
                    "ref": reference.reference,
                    "note": reference.note,
                })
            })
            .collect();
        serde_json::json!({
            "role": self.role,
            "title": self.title,
            "objective": self.objective,
            "output_format": self.output_format.as_str(),
            "scope": {
                "include": self.scope.include,
                "exclude": self.scope.exclude,
                "stop_when": self.scope.stop_when,
            },
            "context_refs": refs,
            "budget": {
                "max_tokens": self.budget.max_tokens,
                "max_provider_requests": self.budget.max_provider_requests,
                "timeout_ms": self.budget.timeout_ms,
                "max_iterations": self.budget.max_iterations,
            },
            "model": self.model,
        })
    }

    /// Size and shape limits. Run for every brief, parsed or built in code,
    /// so no caller can smuggle an oversized or malformed field past the
    /// spawn.
    pub fn validate(&self) -> Result<(), BriefError> {
        if self.role.trim().is_empty() {
            return Err(missing("role"));
        }
        if self.title.trim().is_empty() {
            return Err(missing("title"));
        }
        let title_chars = self.title.chars().count();
        if title_chars > TITLE_MAX_CHARS {
            return Err(invalid(format!(
                "field \"title\" is {title_chars} characters; at most {TITLE_MAX_CHARS}"
            )));
        }
        if self.title.chars().any(char::is_control) {
            return Err(wrong("title", "one line without control characters"));
        }
        if self.objective.trim().is_empty() {
            return Err(missing("objective"));
        }
        if self.objective.chars().count() > OBJECTIVE_MAX_CHARS {
            return Err(too_long("objective", OBJECTIVE_MAX_CHARS));
        }
        if self.scope.stop_when.trim().is_empty() {
            return Err(missing("scope.stop_when"));
        }
        if self.scope.stop_when.chars().count() > STOP_WHEN_MAX_CHARS {
            return Err(too_long("scope.stop_when", STOP_WHEN_MAX_CHARS));
        }
        check_items("scope.include", &self.scope.include)?;
        check_items("scope.exclude", &self.scope.exclude)?;
        if self.context_refs.len() > REFS_MAX {
            return Err(invalid(format!(
                "field \"context_refs\" has {} entries; at most {REFS_MAX}",
                self.context_refs.len()
            )));
        }
        for (index, reference) in self.context_refs.iter().enumerate() {
            check_ref(index, reference)?;
        }
        Ok(())
    }
}

fn too_long(field: &str, max: usize) -> BriefError {
    invalid(format!("field \"{field}\" is longer than {max} characters"))
}

fn check_items(field: &str, items: &[String]) -> Result<(), BriefError> {
    if items.len() > SCOPE_ITEMS_MAX {
        return Err(invalid(format!(
            "field \"{field}\" has {} entries; at most {SCOPE_ITEMS_MAX}",
            items.len()
        )));
    }
    if items
        .iter()
        .any(|item| item.chars().count() > SCOPE_ITEM_MAX_CHARS)
    {
        return Err(too_long(field, SCOPE_ITEM_MAX_CHARS));
    }
    Ok(())
}

/// A record or message id: short, printable, no spaces.
fn is_plain_id(text: &str) -> bool {
    !text.is_empty()
        && text.chars().count() <= REF_ID_MAX_CHARS
        && text
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | ':' | '.'))
}

/// A memory record id as the memory service writes it: a hyphenated UUID.
fn is_record_id(text: &str) -> bool {
    text.len() == 36
        && text.char_indices().all(|(at, ch)| match at {
            8 | 13 | 18 | 23 => ch == '-',
            _ => ch.is_ascii_hexdigit(),
        })
}

fn check_ref(index: usize, reference: &ContextRef) -> Result<(), BriefError> {
    let field = format!("context_refs[{index}].ref");
    let valid = match reference.kind {
        RefKind::Output => OutputHandle::parse(&reference.reference).is_ok(),
        RefKind::Memory => is_record_id(&reference.reference),
        RefKind::Message => is_plain_id(&reference.reference),
        RefKind::Path => {
            !reference.reference.trim().is_empty()
                && reference.reference.chars().count() <= PATH_REF_MAX_CHARS
                && !reference.reference.contains('\0')
        }
    };
    if !valid {
        let expected = match reference.kind {
            RefKind::Output => "an output handle such as out_0191b2c3d4e57f8a9b0c1d2e3f4a5b6c",
            RefKind::Path => "a non-empty workspace path",
            RefKind::Memory => "a memory record id (a hyphenated UUID)",
            RefKind::Message => "a message id",
        };
        return Err(wrong(&field, expected));
    }
    if reference
        .note
        .as_ref()
        .is_some_and(|note| note.chars().count() > NOTE_MAX_CHARS)
    {
        return Err(too_long(
            &format!("context_refs[{index}].note"),
            NOTE_MAX_CHARS,
        ));
    }
    Ok(())
}

fn required_text(object: &Value, key: &str, path: &str) -> Result<String, BriefError> {
    match object.get(key) {
        None | Some(Value::Null) => Err(missing(path)),
        Some(Value::String(text)) if text.trim().is_empty() => Err(missing(path)),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(_) => Err(wrong(path, "a string")),
    }
}

fn optional_text(object: &Value, key: &str, path: &str) -> Result<Option<String>, BriefError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.trim().is_empty() => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(wrong(path, "a string")),
    }
}

fn text_list(value: Option<&Value>, path: &str) -> Result<Vec<String>, BriefError> {
    match value {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| wrong(path, "a list of strings"))
            })
            .collect(),
        Some(_) => Err(wrong(path, "a list of strings")),
    }
}

fn parse_refs(value: Option<&Value>) -> Result<Vec<ContextRef>, BriefError> {
    let items = match value {
        None | Some(Value::Null) => return Ok(vec![]),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(wrong("context_refs", "a list")),
    };
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            if !item.is_object() {
                return Err(wrong(&format!("context_refs[{index}]"), "an object"));
            }
            let kind_text = required_text(item, "kind", &format!("context_refs[{index}].kind"))?;
            let kind = RefKind::parse(&kind_text).ok_or_else(|| {
                wrong(
                    &format!("context_refs[{index}].kind"),
                    &format!("one of {}", RefKind::NAMES.join(", ")),
                )
            })?;
            Ok(ContextRef {
                kind,
                reference: required_text(item, "ref", &format!("context_refs[{index}].ref"))?,
                note: optional_text(item, "note", &format!("context_refs[{index}].note"))?,
            })
        })
        .collect()
}

fn parse_budget(value: Option<&Value>) -> Result<BriefBudget, BriefError> {
    let object = match value {
        None | Some(Value::Null) => return Ok(BriefBudget::default()),
        Some(object) if object.is_object() => object,
        Some(_) => return Err(wrong("budget", "an object")),
    };
    let count = |key: &str| -> Result<Option<u64>, BriefError> {
        match object.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(number) => number
                .as_u64()
                .map(Some)
                .ok_or_else(|| wrong(&format!("budget.{key}"), "a non-negative integer")),
        }
    };
    // `timeout_ms` is the spec's name; the tool schema says `timeout`.
    let timeout_ms = match count("timeout")? {
        Some(value) => Some(value),
        None => count("timeout_ms")?,
    };
    let max_iterations = count("max_iterations")?
        .map(|value| {
            u32::try_from(value).map_err(|_| wrong("budget.max_iterations", "a smaller integer"))
        })
        .transpose()?;
    Ok(BriefBudget {
        max_tokens: count("max_tokens")?,
        max_provider_requests: count("max_provider_requests")?,
        timeout_ms,
        max_iterations,
    })
}

// -- context references ---------------------------------------------------------

/// A reference ready to render: the message text is already copied and
/// capped, the path already confined.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedRef {
    pub kind: RefKind,
    pub reference: String,
    pub note: Option<String>,
    /// Copied text of a `message` reference.
    pub text: Option<String>,
    /// The copied text was cut at [`MESSAGE_REF_CAP_BYTES`].
    pub cut: bool,
}

/// Cut `text` to at most [`MESSAGE_REF_CAP_BYTES`] bytes on a character
/// boundary; the flag says whether anything was cut.
pub fn cap_message_text(text: &str) -> (String, bool) {
    if text.len() <= MESSAGE_REF_CAP_BYTES {
        return (text.to_owned(), false);
    }
    let mut end = MESSAGE_REF_CAP_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `path` with symlinks resolved on its deepest existing ancestor and the
/// not-yet-existing remainder appended again, so a link in the middle of a
/// path to a file that does not exist yet still shows where it leads. `None`
/// when no ancestor resolves.
fn resolve_deepest_existing(path: &Path) -> Option<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut remainder = Vec::new();
    let mut real = loop {
        match existing.canonicalize() {
            Ok(real) => break real,
            Err(_) => {
                remainder.push(existing.file_name()?.to_owned());
                if !existing.pop() {
                    return None;
                }
            }
        }
    };
    for part in remainder.iter().rev() {
        real.push(part);
    }
    Some(real)
}

/// Confine a `path` reference to the inherited workspace. A relative path is
/// taken from the root; `..` segments are resolved before the check, and a
/// path is also checked after symlinks resolve (its deepest existing
/// ancestor, so a file that does not exist yet counts too), so neither a
/// `..` nor a link can leave the root. Returns the path relative to the root
/// (`.` for the root itself).
pub fn confine_to_workspace(root: &str, reference: &str) -> Result<String, String> {
    let root_path = lexical_normalize(Path::new(root));
    let requested = Path::new(reference);
    let joined = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root_path.join(requested)
    };
    let normal = lexical_normalize(&joined);
    let relative = normal
        .strip_prefix(&root_path)
        .map_err(|_| format!("path {reference} lies outside the workspace"))?
        .to_path_buf();
    if let Ok(real_root) = root_path.canonicalize()
        && !resolve_deepest_existing(&normal).is_some_and(|real| real.starts_with(&real_root))
    {
        return Err(format!("path {reference} lies outside the workspace"));
    }
    if relative.as_os_str().is_empty() {
        Ok(".".to_owned())
    } else {
        Ok(relative.display().to_string())
    }
}

// -- the template -----------------------------------------------------------------

/// The limits the child actually runs under, shown in the brief. An absent
/// value renders as `default`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BudgetLines {
    pub max_iterations: Option<u64>,
    pub max_provider_requests: Option<u64>,
    pub timeout_ms: Option<u64>,
    pub max_tokens: Option<u64>,
}

fn limit_text(value: Option<u64>, unit: &str) -> String {
    match value {
        Some(value) => format!("{value}{unit}"),
        None => "default".to_owned(),
    }
}

fn push_list(out: &mut String, label: &str, items: &[String]) {
    out.push_str(label);
    out.push_str(":\n");
    if items.is_empty() {
        out.push_str("- (nothing listed)\n");
    }
    for item in items {
        out.push_str("- ");
        out.push_str(item.trim());
        out.push('\n');
    }
}

/// The child's first message: role prompt, objective, scope, expected output
/// format, references, budget, in that order, with fixed headings. Everything
/// but the brief's own values is constant per role (and per output format),
/// which keeps sibling prefixes aligned.
pub fn render_brief(
    role_prompt: &str,
    brief: &Brief,
    refs: &[RenderedRef],
    budget: &BudgetLines,
) -> String {
    let mut out = String::new();
    if !role_prompt.trim().is_empty() {
        out.push_str(role_prompt.trim());
        out.push_str("\n\n");
    }
    out.push_str("## Objective\n");
    out.push_str(brief.objective.trim());
    out.push_str("\n\n## Scope\n");
    push_list(&mut out, "Include", &brief.scope.include);
    push_list(&mut out, "Exclude", &brief.scope.exclude);
    out.push_str("Stop when: ");
    out.push_str(brief.scope.stop_when.trim());
    out.push_str("\n\n## Expected output\n");
    out.push_str("Format: ");
    out.push_str(brief.output_format.as_str());
    out.push('\n');
    out.push_str(brief.output_format.instructions());
    out.push_str("\n\n## References\n");
    if refs.is_empty() {
        out.push_str("- (none)\n");
    }
    for reference in refs {
        out.push_str("- ");
        out.push_str(reference.kind.as_str());
        out.push(' ');
        out.push_str(&reference.reference);
        if let Some(note) = &reference.note {
            out.push_str(": ");
            out.push_str(note.trim());
        }
        out.push('\n');
        if let Some(text) = &reference.text {
            for line in text.lines() {
                out.push_str("  > ");
                out.push_str(line);
                out.push('\n');
            }
            if reference.cut {
                out.push_str("  > [cut at 4096 bytes]\n");
            }
        }
    }
    out.push_str("\n## Budget\n");
    out.push_str(&format!(
        "- Tool iterations: {}\n- Provider requests: {}\n- Time limit: {}\n- Tokens: {}\n",
        limit_text(budget.max_iterations, ""),
        limit_text(budget.max_provider_requests, ""),
        limit_text(budget.timeout_ms, " ms"),
        limit_text(budget.max_tokens, ""),
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input() -> Value {
        json!({
            "role": "researcher", "title": "Find retries", "objective": "Find retry config.",
            "output_format": "summary", "scope": {"stop_when": "all settings found"},
        })
    }

    #[test]
    fn a_minimal_brief_parses() {
        let brief = Brief::from_input(&input()).unwrap();
        assert_eq!(brief.role, "researcher");
        assert_eq!(brief.output_format, OutputFormat::Summary);
        assert!(brief.context_refs.is_empty());
    }

    #[test]
    fn each_missing_required_field_is_named() {
        for (path, field) in [
            ("role", "role"),
            ("title", "title"),
            ("objective", "objective"),
            ("output_format", "output_format"),
            ("scope", "scope.stop_when"),
        ] {
            let mut value = input();
            value.as_object_mut().unwrap().remove(path);
            let error = Brief::from_input(&value).unwrap_err().to_string();
            assert!(error.contains(&format!("\"{field}\"")), "{path}: {error}");
        }
        let mut value = input();
        value["scope"] = json!({"include": ["src"]});
        let error = Brief::from_input(&value).unwrap_err().to_string();
        assert!(error.contains("scope.stop_when"), "{error}");
        value["scope"] = json!({"stop_when": "  "});
        assert!(Brief::from_input(&value).is_err());
    }

    #[test]
    fn the_title_is_capped_at_eighty_characters() {
        let mut value = input();
        value["title"] = json!("x".repeat(80));
        assert!(Brief::from_input(&value).is_ok());
        value["title"] = json!("x".repeat(81));
        let error = Brief::from_input(&value).unwrap_err().to_string();
        assert!(error.contains("title") && error.contains("81"), "{error}");
    }

    #[test]
    fn reference_shapes_are_checked() {
        let handle = "out_0191b2c3d4e57f8a9b0c1d2e3f4a5b6c";
        let mut value = input();
        value["context_refs"] = json!([
            {"kind": "output", "ref": handle, "note": "log"},
            {"kind": "path", "ref": "src/lib.rs"},
            {"kind": "memory", "ref": "0191b2c3-d4e5-7f8a-9b0c-1d2e3f4a5b6c"},
            {"kind": "message", "ref": "msg_01"},
        ]);
        assert_eq!(Brief::from_input(&value).unwrap().context_refs.len(), 4);
        for bad in [
            json!({"kind": "output", "ref": "not-a-handle"}),
            json!({"kind": "memory", "ref": "has space"}),
            json!({"kind": "memory", "ref": "mem_01"}),
            json!({"kind": "memory", "ref": "0191b2c3d4e57f8a9b0c1d2e3f4a5b6c"}),
            json!({"kind": "tool", "ref": "x"}),
            json!({"kind": "path"}),
        ] {
            value["context_refs"] = json!([bad]);
            assert!(Brief::from_input(&value).is_err(), "{bad}");
        }
    }

    #[test]
    fn message_text_is_capped_on_a_character_boundary() {
        let (kept, cut) = cap_message_text(&"é".repeat(3000));
        assert!(cut && kept.len() <= MESSAGE_REF_CAP_BYTES);
        let (whole, cut) = cap_message_text("short");
        assert!(!cut && whole == "short");
    }

    #[test]
    fn paths_must_stay_inside_the_workspace() {
        assert_eq!(confine_to_workspace("/ws", "src/a.rs").unwrap(), "src/a.rs");
        assert_eq!(
            confine_to_workspace("/ws", "/ws/src/a.rs").unwrap(),
            "src/a.rs"
        );
        assert_eq!(confine_to_workspace("/ws", ".").unwrap(), ".");
        assert!(confine_to_workspace("/ws", "../etc/passwd").is_err());
        assert!(confine_to_workspace("/ws", "/etc/passwd").is_err());
        assert!(confine_to_workspace("/ws", "src/../../x").is_err());
        assert!(confine_to_workspace("/ws", "/ws-other/x").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_cannot_lead_out_through_a_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let root = root.to_str().unwrap();
        assert!(confine_to_workspace(root, "link/not-yet.txt").is_err());
        assert!(confine_to_workspace(root, "link/deeper/not-yet.txt").is_err());
        assert!(confine_to_workspace(root, "link").is_err());
        assert_eq!(
            confine_to_workspace(root, "src/not-yet.txt").unwrap(),
            "src/not-yet.txt"
        );
    }

    #[test]
    fn the_digest_follows_every_brief_field() {
        let brief = Brief::new("researcher", "t", "find it", OutputFormat::Summary, "done");
        assert_eq!(brief.digest(), brief.clone().digest());
        let mut other = brief.clone();
        other.objective = "find something else".into();
        assert_ne!(brief.digest(), other.digest());
    }

    #[test]
    fn to_input_round_trips_every_field() {
        let mut value = input();
        value["scope"] = json!({"include": ["src"], "exclude": ["tests"], "stop_when": "done"});
        value["context_refs"] = json!([{"kind": "path", "ref": "src/lib.rs", "note": "start"}]);
        value["budget"] = json!({"max_tokens": 900, "timeout_ms": 5000, "max_iterations": 3});
        value["model"] = json!("small-model");
        let brief = Brief::from_input(&value).unwrap();
        assert_eq!(Brief::from_input(&brief.to_input()).unwrap(), brief);
    }
}
