//! ECMAScript property matching with shared, eagerly compiled root-reference context.
use crate::{
    ValidationError,
    schema_mirror::{pointer, tool_schema_mirror},
};
use jsonschema::{
    Draft, Keyword, Validator,
    paths::{LazyLocation, Location},
};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Deref,
    sync::{
        Arc, RwLock, Weak,
        atomic::{AtomicUsize, Ordering},
    },
};

/// A validator retaining the shared context used by custom property keywords.
pub struct SchemaValidator {
    inner: Validator,
    _context: Arc<Context>,
}
impl Deref for SchemaValidator {
    type Target = Validator;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
struct Context {
    root: Value,
    root_uri: String,
    entry_uri: String,
    marker: String,
    tools: bool,
    draft: Draft,
    children: RwLock<BTreeMap<String, Arc<Validator>>>,
    compilations: AtomicUsize,
}

pub(crate) fn compile(schema: &Value, tools: bool) -> Result<SchemaValidator, ValidationError> {
    let draft = if tools {
        crate::tool_schema_profile::draft(schema)?
    } else {
        Draft::Draft7
    };
    let (mut root, marker) = tool_schema_mirror(schema);
    let namespace = private_namespace(schema);
    let root_uri = resolve_root_id(&mut root, &namespace)?;
    let context = Arc::new(Context {
        root,
        root_uri,
        entry_uri: format!("{namespace}entry"),
        marker,
        tools,
        draft,
        children: RwLock::new(BTreeMap::new()),
        compilations: AtomicUsize::new(0),
    });
    let inner = context.compile_at("")?;
    for path in child_paths(&context.root, &context.marker) {
        let compiled = context.compile_at(&path)?;
        context
            .children
            .write()
            .expect("schema cache lock")
            .insert(path, Arc::new(compiled));
    }
    Ok(SchemaValidator {
        inner,
        _context: context,
    })
}

impl Context {
    // The callback signature fixes the borrowed ValidationError return type.
    #[allow(clippy::result_large_err)]
    fn compile_at(self: &Arc<Self>, pointer: &str) -> Result<Validator, ValidationError> {
        self.compilations.fetch_add(1, Ordering::Relaxed);
        let resource = self.draft.create_resource(self.root.clone());
        let weak = Arc::downgrade(self);
        let additional_weak = weak.clone();
        let options = crate::ajv::profile_options(self.marker.clone(), self.tools)
            .with_draft(self.draft)
            .with_resource(&self.root_uri, resource)
            .with_base_uri(&self.entry_uri)
            .with_keyword("patternProperties", move |parent, value, path| {
                property_keyword(parent, value, path, weak.clone(), false)
            })
            .with_keyword("additionalProperties", move |parent, value, path| {
                property_keyword(parent, value, path, additional_weak.clone(), true)
            });
        options
            .build(&json!({"$ref":format!("{}#{}", self.root_uri, fragment(pointer))}))
            .map_err(invalid)
    }
    fn child(&self, pointer: &str) -> Option<Arc<Validator>> {
        self.children
            .read()
            .expect("schema cache lock")
            .get(pointer)
            .cloned()
    }
}
fn private_namespace(schema: &Value) -> String {
    let source = schema.to_string();
    for suffix in 0_u64.. {
        let candidate = format!("https://agentkit-compiled.invalid/{suffix}/");
        if !source.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!("a finite schema cannot contain every namespace")
}
fn resolve_root_id(root: &mut Value, namespace: &str) -> Result<String, ValidationError> {
    let base = format!("{namespace}root.json");
    let Some(id) = root.get("$id").and_then(Value::as_str) else {
        return Ok(base);
    };
    let uri = fluent_uri::UriRef::parse(id)
        .map_err(invalid)?
        .resolve_against(&fluent_uri::Uri::parse(base.as_str()).map_err(invalid)?)
        .map_err(invalid)?
        .normalize()
        .to_string();
    root["$id"] = json!(uri);
    Ok(uri)
}
fn fragment(pointer: &str) -> String {
    use std::fmt::Write;
    let mut encoded = String::new();
    for byte in pointer.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            write!(encoded, "%{byte:02X}").expect("writing to string");
        }
    }
    encoded
}
fn invalid(error: impl std::fmt::Display) -> ValidationError {
    ValidationError::Invalid(error.to_string())
}

fn child_paths(value: &Value, marker: &str) -> BTreeSet<String> {
    let mut output = BTreeSet::new();
    collect_paths(value, marker, &mut output);
    output
}
fn collect_paths(value: &Value, marker: &str, paths: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            if let Some(location) = object
                .get(marker)
                .and_then(|value| value["location"].as_str())
            {
                if let Some(patterns) = object.get("patternProperties").and_then(Value::as_object) {
                    for key in patterns.keys() {
                        paths.insert(pointer(&pointer(location, "patternProperties"), key));
                    }
                }
                if object
                    .get("additionalProperties")
                    .is_some_and(Value::is_object)
                {
                    paths.insert(pointer(location, "additionalProperties"));
                }
                if matches!(
                    object.get("additionalProperties"),
                    Some(Value::Bool(false) | Value::Object(_))
                ) && let Some(properties) = object.get("properties").and_then(Value::as_object)
                {
                    for name in properties.keys() {
                        paths.insert(pointer(&pointer(location, "properties"), name));
                    }
                }
            }
            object
                .values()
                .for_each(|value| collect_paths(value, marker, paths));
        }
        Value::Array(items) => items
            .iter()
            .for_each(|value| collect_paths(value, marker, paths)),
        _ => {}
    }
}

struct PropertyKeyword {
    path: Location,
    context: Weak<Context>,
    patterns: Vec<Pattern>,
    additional: Option<Additional>,
}
struct Pattern {
    regex: regress::Regex,
    /// The pattern as written, for error messages.
    source: String,
    /// Pointer to the pattern's schema in the root.
    path: String,
}
struct Additional {
    declared: BTreeMap<String, String>,
    rule: AdditionalRule,
}
enum AdditionalRule {
    Allow,
    Deny,
    Schema(String),
}
impl Keyword for PropertyKeyword {
    fn is_valid(&self, instance: &Value) -> bool {
        let Some(object) = instance.as_object() else {
            return true;
        };
        let Some(context) = self.context.upgrade() else {
            return false;
        };
        object
            .iter()
            .all(|(name, value)| self.rejection(&context, name, value).is_none())
    }
    fn validate<'i>(
        &self,
        instance: &'i Value,
        location: &LazyLocation<'_, '_>,
    ) -> Result<(), jsonschema::ValidationError<'i>> {
        if self.is_valid(instance) {
            return Ok(());
        }
        // The verdict above is final. What follows only names the property
        // that failed and why, so a model can fix its call (TU-10, H1.6).
        let object_path = Location::from(location);
        Err(self.describe(instance, &object_path).unwrap_or_else(|| {
            jsonschema::ValidationError::custom(
                self.path.clone(),
                object_path,
                instance,
                "property rejected by ECMAScript schema constraints",
            )
        }))
    }
}
/// Why one property of an object fails this keyword.
enum Rejection {
    /// Not declared, matched by no pattern, and the object is closed.
    NotAllowed,
    /// Its value fails the schema that applies to it.
    Value(Arc<Validator>),
    /// The schema that applies to it is not compiled.
    Unchecked,
}
impl PropertyKeyword {
    /// `None` when the property passes this keyword.
    fn rejection(&self, context: &Context, name: &str, value: &Value) -> Option<Rejection> {
        let check = |path: &str| match context.child(path) {
            Some(validator) if validator.is_valid(value) => None,
            Some(validator) => Some(Rejection::Value(validator)),
            None => Some(Rejection::Unchecked),
        };
        let mut matches = self
            .patterns
            .iter()
            .filter(|pattern| pattern.regex.find(name).is_some());
        if let Some(additional) = &self.additional {
            if let Some(path) = additional.declared.get(name) {
                // jsonschema delegates declared-property checks to additionalProperties
                // when its value is false or a schema, so the override must retain them.
                return match additional.rule {
                    AdditionalRule::Allow => None,
                    _ => check(path),
                };
            }
            if matches.next().is_some() {
                return None;
            }
            return match &additional.rule {
                AdditionalRule::Allow => None,
                AdditionalRule::Deny => Some(Rejection::NotAllowed),
                AdditionalRule::Schema(path) => check(path),
            };
        }
        matches.find_map(|pattern| check(&pattern.path))
    }

    /// The error for the first rejected property, in the caller's order. A
    /// bad value is the nested validator's own error at its full path; an
    /// undeclared property is named with every other undeclared one and the
    /// properties the object accepts.
    fn describe<'i>(
        &self,
        instance: &'i Value,
        object_path: &Location,
    ) -> Option<jsonschema::ValidationError<'i>> {
        let object = instance.as_object()?;
        let context = self.context.upgrade()?;
        let rejected: Vec<(&String, &'i Value, Rejection)> = object
            .iter()
            .filter_map(|(name, value)| {
                self.rejection(&context, name, value)
                    .map(|rejection| (name, value, rejection))
            })
            .collect();
        let (name, value, rejection) = rejected.first()?;
        let path = object_path.join(name.as_str());
        let message = match rejection {
            Rejection::Value(validator) => {
                if let Some(error) = validator.iter_errors(value).next() {
                    let mut error = error.to_owned();
                    error.instance_path = appended(&path, &error.instance_path);
                    return Some(error);
                }
                format!("property \"{name}\" has a value its schema rejects")
            }
            Rejection::Unchecked => format!("property \"{name}\" could not be checked"),
            Rejection::NotAllowed => {
                let extra: Vec<&str> = rejected
                    .iter()
                    .filter(|(_, _, rejection)| matches!(rejection, Rejection::NotAllowed))
                    .map(|(name, _, _)| name.as_str())
                    .collect();
                self.not_allowed(&extra)
            }
        };
        Some(jsonschema::ValidationError::custom(
            self.path.clone(),
            path,
            value,
            message,
        ))
    }

    fn not_allowed(&self, extra: &[&str]) -> String {
        let quoted = |names: &mut dyn Iterator<Item = &str>| {
            names
                .map(|name| format!("\"{name}\""))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let subject = match extra {
            [one] => format!("property \"{one}\" is not allowed here"),
            many => format!(
                "properties {} are not allowed here",
                quoted(&mut many.iter().copied())
            ),
        };
        let declared = quoted(
            &mut self
                .additional
                .iter()
                .flat_map(|additional| additional.declared.keys().map(String::as_str)),
        );
        let patterns = quoted(&mut self.patterns.iter().map(|pattern| pattern.source.as_str()));
        let allowed = match (declared.is_empty(), patterns.is_empty()) {
            (true, true) => "this object takes no properties".to_owned(),
            (false, true) => format!("the allowed properties are {declared}"),
            (true, false) => format!("allowed are names matching {patterns}"),
            (false, false) => {
                format!("the allowed properties are {declared}, and names matching {patterns}")
            }
        };
        let pronoun = if extra.len() == 1 { "it" } else { "them" };
        format!("{subject}; {allowed}. Remove {pronoun} or use an allowed property")
    }
}

/// `base` followed by the segments of the JSON pointer `tail`.
fn appended(base: &Location, tail: &Location) -> Location {
    tail.as_str()
        .split('/')
        .skip(1)
        .fold(base.clone(), |path, segment| {
            path.join(segment.replace("~1", "/").replace("~0", "~").as_str())
        })
}

// The third-party keyword factory API fixes the large borrowed error type.
#[allow(clippy::result_large_err)]
fn property_keyword<'a>(
    parent: &'a Map<String, Value>,
    value: &'a Value,
    path: Location,
    weak: Weak<Context>,
    additional: bool,
) -> Result<Box<dyn Keyword>, jsonschema::ValidationError<'a>> {
    let context = weak.upgrade().expect("compiler owns schema context");
    let failure = |message: String| {
        jsonschema::ValidationError::custom(path.clone(), Location::new(), value, message)
    };
    let location = parent
        .get(&context.marker)
        .and_then(|value| value["location"].as_str())
        .ok_or_else(|| failure("missing schema location".into()))?;
    let mut patterns = Vec::new();
    if let Some(map) = parent.get("patternProperties").and_then(Value::as_object) {
        for pattern in map.keys() {
            let regex = regress::Regex::with_flags(pattern, "u")
                .map_err(|error| failure(error.to_string()))?;
            patterns.push(Pattern {
                regex,
                source: pattern.clone(),
                path: pointer(&pointer(location, "patternProperties"), pattern),
            });
        }
    }
    let additional = if additional {
        let rule = match value {
            Value::Bool(true) => AdditionalRule::Allow,
            Value::Bool(false) => AdditionalRule::Deny,
            Value::Object(_) => AdditionalRule::Schema(pointer(location, "additionalProperties")),
            _ => return Err(failure("invalid additionalProperties".into())),
        };
        let declared = parent
            .get("properties")
            .and_then(Value::as_object)
            .map(|properties| {
                properties
                    .keys()
                    .map(|name| {
                        (
                            name.clone(),
                            pointer(&pointer(location, "properties"), name),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(Additional { declared, rule })
    } else {
        None
    };
    Ok(Box::new(PropertyKeyword {
        path,
        context: weak,
        patterns,
        additional,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_reuses_compiled_children_without_retaining_a_cycle() {
        let validator = compile(
            &json!({
                "type":"object", "patternProperties":{"^x$":{"$ref":"#"}},
                "additionalProperties":false
            }),
            true,
        )
        .unwrap();
        let context = Arc::downgrade(&validator._context);
        let compilations = validator._context.compilations.load(Ordering::Relaxed);
        for _ in 0..20 {
            assert!(validator.is_valid(&json!({"x":{"x":{}}})));
            assert!(!validator.is_valid(&json!({"x":{"unexpected":true}})));
        }
        assert_eq!(
            validator._context.compilations.load(Ordering::Relaxed),
            compilations
        );
        drop(validator);
        assert!(context.upgrade().is_none());
    }
}
