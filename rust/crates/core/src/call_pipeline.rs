//! Call pipeline stages that run before validation (TU-08): resolve the name,
//! then normalise aliases and text-typed values. Guards and execution see the
//! canonical call this module produces. Normalisation never invents a value:
//! it maps known aliases and converts values the schema asks to be another
//! type, and it reports every case it touched.
use crate::{CoreError, ToolCall, ToolRegistry, envelope};
use serde_json::{Map, Value};
use std::collections::HashMap;

/// Placeholder name for an emitted call whose name never arrived. Providers
/// use it so the call still gets a result; the pipeline refuses it with a
/// message that says the name was missing.
pub const UNNAMED_TOOL: &str = "unnamed_call";

const MAX_COERCE_DEPTH: usize = 8;
const CLOSEST_NAMES: usize = 3;

/// One alias: a parameter name models send instead of the canonical one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AliasRule {
    pub alias: String,
    pub canonical: String,
    /// The value is multiplied by this factor (`timeout_sec` -> `timeout`).
    pub scale: Option<u64>,
}

/// Alias data. `standard()` is the spec's first table; per-tool renames
/// register extra entries with `add_for_tool`.
#[derive(Clone, Debug)]
pub struct AliasTable {
    global: Vec<AliasRule>,
    per_tool: HashMap<String, Vec<AliasRule>>,
}

impl Default for AliasTable {
    fn default() -> Self {
        Self::standard()
    }
}

impl AliasTable {
    pub fn empty() -> Self {
        Self {
            global: vec![],
            per_tool: HashMap::new(),
        }
    }

    pub fn standard() -> Self {
        let mut table = Self::empty();
        for (aliases, canonical) in [
            (&["path", "filePath", "filepath", "file"][..], "file_path"),
            (&["old_str", "oldText"][..], "old_string"),
            (&["new_str", "newText"][..], "new_string"),
            (&["cmd"][..], "command"),
            (&["cwd", "workdir"][..], "directory"),
            (&["timeout_ms"][..], "timeout"),
        ] {
            for alias in aliases {
                table.add(alias, canonical);
            }
        }
        table.add_scaled("timeout_sec", "timeout", 1000);
        table.add_scaled("timeoutSec", "timeout", 1000);
        table
    }

    pub fn add(&mut self, alias: &str, canonical: &str) {
        self.global.push(rule(alias, canonical, None));
    }

    pub fn add_scaled(&mut self, alias: &str, canonical: &str, scale: u64) {
        self.global.push(rule(alias, canonical, Some(scale)));
    }

    /// An alias that applies to one tool only (a renamed parameter).
    pub fn add_for_tool(&mut self, tool: &str, alias: &str, canonical: &str) {
        self.per_tool
            .entry(tool.into())
            .or_default()
            .push(rule(alias, canonical, None));
    }

    fn rules<'a>(&'a self, tool: &str) -> impl Iterator<Item = &'a AliasRule> {
        self.per_tool
            .get(tool)
            .into_iter()
            .flatten()
            .chain(self.global.iter())
    }
}

fn rule(alias: &str, canonical: &str, scale: Option<u64>) -> AliasRule {
    AliasRule {
        alias: alias.into(),
        canonical: canonical.into(),
        scale,
    }
}

/// Arguments after normalisation, with what was done to them.
#[derive(Clone, Debug)]
pub struct Normalised {
    pub args: Value,
    /// `alias->canonical` (with `*N` when scaled), one entry per applied alias.
    pub aliases_used: Vec<String>,
    pub values_converted: u32,
}

/// Aliases first, then type conversion. Only objects are touched.
pub fn normalise(table: &AliasTable, tool: &str, schema: &Value, args: Value) -> Normalised {
    let mut out = Normalised {
        args,
        aliases_used: vec![],
        values_converted: 0,
    };
    let Some(object) = out.args.as_object_mut() else {
        return out;
    };
    let properties = schema["properties"].as_object();
    if let Some(properties) = properties {
        for rule in table.rules(tool) {
            apply_alias(rule, properties, object, &mut out.aliases_used);
        }
    }
    let mut converted = 0;
    coerce_object(schema, object, 0, &mut converted);
    out.values_converted = converted;
    out
}

fn apply_alias(
    rule: &AliasRule,
    properties: &Map<String, Value>,
    object: &mut Map<String, Value>,
    used: &mut Vec<String>,
) {
    // The alias is itself a real parameter of this tool: not an alias here.
    if !properties.contains_key(&rule.canonical)
        || properties.contains_key(&rule.alias)
        || object.contains_key(&rule.canonical)
    {
        return;
    }
    let Some(value) = object.get(&rule.alias) else {
        return;
    };
    let moved = match rule.scale {
        None => value.clone(),
        Some(scale) => match scaled(value, scale) {
            Some(value) => value,
            None => return,
        },
    };
    object.remove(&rule.alias);
    object.insert(rule.canonical.clone(), moved);
    used.push(match rule.scale {
        None => format!("{}->{}", rule.alias, rule.canonical),
        Some(scale) => format!("{}->{}*{scale}", rule.alias, rule.canonical),
    });
}

/// `value * scale` when `value` is a whole non-negative number or text that
/// parses as one. Anything else is left for validation to report.
fn scaled(value: &Value, scale: u64) -> Option<Value> {
    let number = match value {
        Value::Number(number) => number.as_u64().or_else(|| {
            number
                .as_f64()
                .filter(|float| *float >= 0.0 && float.fract() == 0.0)
                .map(|float| float as u64)
        })?,
        Value::String(text) => text.parse::<u64>().ok()?,
        _ => return None,
    };
    number.checked_mul(scale).map(Value::from)
}

fn coerce_object(schema: &Value, object: &mut Map<String, Value>, depth: usize, count: &mut u32) {
    let Some(properties) = schema["properties"].as_object() else {
        return;
    };
    for (key, child_schema) in properties {
        if let Some(value) = object.get_mut(key) {
            coerce_value(child_schema, value, depth + 1, count);
        }
    }
}

fn coerce_value(schema: &Value, value: &mut Value, depth: usize, count: &mut u32) {
    if depth > MAX_COERCE_DEPTH {
        return;
    }
    if let Some(text) = value.as_str() {
        if let Some(converted) = convert_text(schema, text) {
            *value = converted;
            *count += 1;
        }
        return;
    }
    match value {
        Value::Object(object) => coerce_object(schema, object, depth, count),
        Value::Array(items) => {
            for item in items {
                coerce_value(&schema["items"], item, depth + 1, count);
            }
        }
        _ => {}
    }
}

fn schema_types(schema: &Value) -> Vec<&str> {
    match &schema["type"] {
        Value::String(kind) => vec![kind.as_str()],
        Value::Array(kinds) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    }
}

/// Text sent where the schema asks for an integer, boolean, array or object,
/// and that parses exactly as that type. A schema that also accepts text keeps
/// the text.
fn convert_text(schema: &Value, text: &str) -> Option<Value> {
    let kinds = schema_types(schema);
    if kinds.contains(&"string") {
        return None;
    }
    if kinds.contains(&"integer")
        && let Ok(number) = text.parse::<i64>()
    {
        return Some(number.into());
    }
    if kinds.contains(&"boolean") {
        match text {
            "true" => return Some(true.into()),
            "false" => return Some(false.into()),
            _ => {}
        }
    }
    let structured = (kinds.contains(&"array") && text.trim_start().starts_with('['))
        || (kinds.contains(&"object") && text.trim_start().starts_with('{'));
    if structured && let Ok(parsed) = agentkit_contracts::parse_javascript_json(text) {
        let fits = (parsed.is_array() && kinds.contains(&"array"))
            || (parsed.is_object() && kinds.contains(&"object"));
        if fits {
            return Some(parsed);
        }
    }
    None
}

/// Levenshtein distance over chars.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (diagonal + usize::from(ca != *cb))
                .min(above + 1)
                .min(row[j] + 1);
            diagonal = above;
        }
    }
    row[b.len()]
}

/// The `limit` candidates nearest to `name`, closest first, ties by name.
pub fn closest_names(name: &str, candidates: &[String], limit: usize) -> Vec<String> {
    let lowered = name.to_lowercase();
    let mut scored: Vec<(usize, &String)> = candidates
        .iter()
        .map(|candidate| (distance(&lowered, &candidate.to_lowercase()), candidate))
        .collect();
    scored.sort();
    scored
        .into_iter()
        .take(limit)
        .map(|(_, name)| name.clone())
        .collect()
}

/// Stage 1 failure for a name the registry cannot run.
pub(crate) fn resolve_failure(registry: &ToolRegistry, name: &str) -> CoreError {
    // The model reads the names it was offered, not canonical ids (T3.1).
    let names = registry.presented_names();
    let shown_name = registry.present_name(name);
    if name.is_empty() || name == UNNAMED_TOOL {
        let shown: Vec<String> = names.iter().take(8).cloned().collect();
        return CoreError::new(
            "tool_name_missing",
            &format!(
                "The tool call had no name, so it could not run. Call a tool by its exact name{}. If unsure which tool fits, call tool_search.",
                list_suffix(" such as", &shown)
            ),
        );
    }
    let nearest = closest_names(&shown_name, &names, CLOSEST_NAMES);
    if let Some(reason) = registry.withheld_reason(name) {
        let because = if reason.is_empty() {
            String::new()
        } else {
            format!(" ({reason})")
        };
        return CoreError::new(
            "tool_not_allowed",
            &format!(
                "Tool {shown_name} is not available this turn{because}. Use another tool instead{}. Call tool_search to see what is available.",
                list_suffix(": ", &nearest)
            ),
        );
    }
    if registry.is_deferred(name) && registry.meta_active() {
        return CoreError::new(
            "tool_deferred",
            &format!(
                "Tool {shown_name} is deferred: it is not in your tool list. Call it through tool_call with its name and arguments; tool_describe shows its schema."
            ),
        );
    }
    if registry.is_deferred(name) {
        return CoreError::new(
            "tool_unavailable",
            &format!(
                "Tool {shown_name} joined this session after its tool list was fixed, so it cannot be called yet. Use one of the offered tools instead{}.",
                list_suffix(": ", &nearest)
            ),
        );
    }
    if registry.is_unavailable(name) {
        return CoreError::new(
            "tool_unavailable",
            &format!(
                "Tool {shown_name} is in this session's tool list but is not available right now. Use another tool instead{}, or try {shown_name} again in a later turn.",
                list_suffix(": ", &nearest)
            ),
        );
    }
    CoreError::new(
        "tool_missing",
        &format!(
            "Tool not registered: {name}. The closest tools are{}. Call tool_search to find the right one.",
            if nearest.is_empty() {
                " none (no tools are available)".to_owned()
            } else {
                format!(": {}", nearest.join(", "))
            }
        ),
    )
}

fn list_suffix(lead: &str, names: &[String]) -> String {
    if names.is_empty() {
        String::new()
    } else {
        format!("{lead}{}", names.join(", "))
    }
}

/// A call after the pre-validation stages. `call` carries the canonical
/// arguments (the original text when nothing changed).
pub(crate) struct Prepared {
    pub call: ToolCall,
    /// The model's own call when `call` is the inner call of a `tool_call`
    /// (same id). Only its name still matters: the transcript pairs the
    /// result with it.
    pub outer: Option<ToolCall>,
    pub aliases_used: Vec<String>,
    pub values_converted: u32,
    pub failure: Option<CoreError>,
}

pub(crate) fn prepare(registry: &ToolRegistry, call: &ToolCall) -> Prepared {
    if call.name == crate::deferred::TOOL_CALL && registry.meta_active() {
        return crate::meta_call::prepare(registry, call);
    }
    prepare_resolved(registry, call, false)
}

/// The canonical form of `call` under the session's dialect (the call itself
/// without one, or for a name the dialect does not present). A call the
/// dialect cannot translate keeps its arguments and comes back with the
/// refusal the model reads. Arguments that are not JSON are left for the
/// `bad_args` stage, after the name is canonical.
pub(crate) fn translate_call(
    registry: &ToolRegistry,
    call: &ToolCall,
) -> (ToolCall, Option<CoreError>) {
    let Some(dialect) = registry.dialect() else {
        return (call.clone(), None);
    };
    // A strict dialect (T3.3) also reads the arguments of tools it does not
    // rename: optional arguments arrive as null.
    if call.name == UNNAMED_TOOL || !(dialect.knows_wire(&call.name) || dialect.strict()) {
        return (call.clone(), None);
    }
    let mut out = call.clone();
    // A built-in tool that is refused stays under its own name: its history
    // entry and its result must pair as the vendor's call and output items.
    let built_in = dialect.built_in(&call.name);
    if !built_in && let Some(canonical) = dialect.canonical_of(&call.name) {
        out.name = canonical.to_owned();
    }
    let args = match agentkit_contracts::parse_javascript_json(&call.arguments_json) {
        Ok(args) => args,
        Err(error) if built_in => {
            let message = format!(
                "Invalid arguments JSON: {error}. Send the arguments as one JSON object."
            );
            return (out, Some(CoreError::new("bad_args", &message)));
        }
        Err(_) => return (out, None),
    };
    match registry.translate(&call.name, args) {
        Ok(translated) => {
            // A built-in tool (T3.3) reaches more than one canonical tool; the
            // translation names the one this call means.
            out.name = translated.name.clone();
            if translated.args_changed {
                out.arguments_json = envelope::stringify(&translated.args);
            }
            (out, None)
        }
        Err(error) => (out, Some(CoreError::new("bad_args", &error.message))),
    }
}

/// The stages for one tool call. `via_meta` is an inner call of `tool_call`:
/// it may name a deferred tool, which a direct call may not.
pub(crate) fn prepare_resolved(
    registry: &ToolRegistry,
    call: &ToolCall,
    via_meta: bool,
) -> Prepared {
    // Presentation layer first (T3.1): everything below, and every guard
    // after it, sees the canonical call.
    let (call, refused) = translate_call(registry, call);
    let call = &call;
    let mut prepared = Prepared {
        call: call.clone(),
        outer: None,
        aliases_used: vec![],
        values_converted: 0,
        failure: refused,
    };
    if prepared.failure.is_some() {
        return prepared;
    }
    // A retired name maps to its replacement (or teaches) before the
    // unknown-tool failure; guards below see the replacement call.
    let renamed;
    let call = match crate::legacy_name::resolve(registry, call, via_meta) {
        None => call,
        Some(Err(error)) => {
            prepared.failure = Some(error);
            return prepared;
        }
        Some(Ok((mapped, note))) => {
            renamed = mapped;
            prepared.call = renamed.clone();
            prepared.aliases_used.push(note);
            &renamed
        }
    };
    if call.name == UNNAMED_TOOL
        || registry.get(&call.name).is_none()
        || (!via_meta && registry.is_deferred(&call.name))
    {
        prepared.failure = Some(resolve_failure(registry, &call.name));
        return prepared;
    }
    let args = match agentkit_contracts::parse_javascript_json(&call.arguments_json) {
        Ok(args) => args,
        Err(error) => {
            prepared.failure = Some(CoreError::new(
                "bad_args",
                &format!(
                    "Invalid arguments JSON: {error}. Send the arguments as one JSON object."
                ),
            ));
            return prepared;
        }
    };
    let normalised = registry.normalise(&call.name, args);
    if !normalised.aliases_used.is_empty() || normalised.values_converted > 0 {
        prepared.call.arguments_json = envelope::stringify(&normalised.args);
    }
    prepared.aliases_used.extend(normalised.aliases_used);
    prepared.values_converted = normalised.values_converted;
    prepared
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({"type":"object","properties":{
            "file_path":{"type":"string"},"old_string":{"type":"string"},"new_string":{"type":"string"},
            "command":{"type":"string"},"directory":{"type":"string"},"timeout":{"type":"integer"},
            "limit":{"type":"integer"},"all":{"type":"boolean"},"tags":{"type":"array","items":{"type":"integer"}},
            "options":{"type":"object","properties":{"depth":{"type":"integer"}}},"note":{"type":"string"}
        }})
    }
    fn run(args: Value) -> Normalised {
        normalise(&AliasTable::standard(), "t", &schema(), args)
    }

    #[test]
    fn standard_alias_table_maps_every_spec_row() {
        let rows = [
            ("path", "file_path"),
            ("filePath", "file_path"),
            ("filepath", "file_path"),
            ("file", "file_path"),
            ("old_str", "old_string"),
            ("oldText", "old_string"),
            ("new_str", "new_string"),
            ("newText", "new_string"),
            ("cmd", "command"),
            ("cwd", "directory"),
            ("workdir", "directory"),
            ("timeout_ms", "timeout"),
        ];
        for (alias, canonical) in rows {
            let value = if canonical == "timeout" { json!(5) } else { json!("v") };
            let n = run(json!({ alias: value }));
            assert!(n.args.get(canonical).is_some(), "{alias}");
            assert!(n.args.get(alias).is_none(), "{alias}");
            assert_eq!(n.aliases_used, [format!("{alias}->{canonical}")]);
        }
    }

    #[test]
    fn timeout_seconds_are_scaled_to_milliseconds() {
        for alias in ["timeout_sec", "timeoutSec"] {
            let n = run(json!({ alias: 3 }));
            assert_eq!(n.args["timeout"], 3000);
            assert_eq!(n.aliases_used, [format!("{alias}->timeout*1000")]);
        }
        assert_eq!(run(json!({"timeout_sec":"2"})).args["timeout"], 2000);
        // Not a whole number: left alone for validation to report.
        let n = run(json!({"timeout_sec":"soon"}));
        assert!(n.aliases_used.is_empty());
        assert_eq!(n.args["timeout_sec"], "soon");
    }

    #[test]
    fn alias_applies_only_when_canonical_absent_and_in_schema() {
        let n = run(json!({"file_path":"a","path":"b"}));
        assert!(n.aliases_used.is_empty());
        assert_eq!(n.args["file_path"], "a");
        assert_eq!(n.args["path"], "b");
        // Canonical not in this tool's schema: no rewrite.
        let n = normalise(
            &AliasTable::standard(),
            "t",
            &json!({"type":"object","properties":{"url":{"type":"string"}}}),
            json!({"path":"x"}),
        );
        assert!(n.aliases_used.is_empty());
        // The alias is a real parameter of the tool: no rewrite.
        let n = normalise(
            &AliasTable::standard(),
            "t",
            &json!({"type":"object","properties":{"path":{"type":"string"},"file_path":{"type":"string"}}}),
            json!({"path":"x"}),
        );
        assert!(n.aliases_used.is_empty());
    }

    #[test]
    fn per_tool_aliases_register_as_data() {
        let mut table = AliasTable::standard();
        table.add_for_tool("t", "q", "note");
        let n = normalise(&table, "t", &schema(), json!({"q":"hi"}));
        assert_eq!(n.args["note"], "hi");
        assert_eq!(n.aliases_used, ["q->note"]);
        let other = normalise(&table, "other", &schema(), json!({"q":"hi"}));
        assert!(other.aliases_used.is_empty());
    }

    #[test]
    fn text_converts_only_where_the_schema_asks_and_it_parses_exactly() {
        let n = run(json!({"limit":"25","all":"true","tags":"[1,2]","options":"{\"depth\":1}"}));
        assert_eq!(n.args["limit"], 25);
        assert_eq!(n.args["all"], true);
        assert_eq!(n.args["tags"], json!([1, 2]));
        assert_eq!(n.args["options"], json!({"depth":1}));
        assert_eq!(n.values_converted, 4);
        let n = run(json!({"limit":"25x","all":"yes","tags":"1,2","note":"7","options":{"depth":"3"}}));
        assert_eq!(n.args["limit"], "25x");
        assert_eq!(n.args["all"], "yes");
        assert_eq!(n.args["tags"], "1,2");
        assert_eq!(n.args["note"], "7");
        assert_eq!(n.args["options"]["depth"], 3);
        assert_eq!(n.values_converted, 1);
    }

    #[test]
    fn missing_required_values_are_never_filled() {
        let n = run(json!({}));
        assert_eq!(n.args, json!({}));
        assert_eq!(n.values_converted, 0);
    }

    #[test]
    fn closest_names_rank_by_edit_distance() {
        let names: Vec<String> = ["read_file", "write_file", "edit_file", "search", "shell"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            closest_names("raed_file", &names, 3),
            ["read_file", "edit_file", "write_file"]
        );
        assert_eq!(closest_names("x", &[], 3), Vec::<String>::new());
    }
}
