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
    children: RwLock<BTreeMap<String, Arc<Validator>>>,
    compilations: AtomicUsize,
}

pub(crate) fn compile(schema: &Value, tools: bool) -> Result<SchemaValidator, ValidationError> {
    let (mut root, marker) = tool_schema_mirror(schema);
    let namespace = private_namespace(schema);
    let root_uri = resolve_root_id(&mut root, &namespace)?;
    let context = Arc::new(Context {
        root,
        root_uri,
        entry_uri: format!("{namespace}entry"),
        marker,
        tools,
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
        let resource = Draft::Draft7.create_resource(self.root.clone());
        let weak = Arc::downgrade(self);
        let additional_weak = weak.clone();
        let options = crate::ajv::profile_options(self.marker.clone(), self.tools)
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
    patterns: Vec<(regress::Regex, String)>,
    additional: Option<Additional>,
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
            .all(|(name, value)| self.valid_property(&context, name, value))
    }
    fn validate<'i>(
        &self,
        instance: &'i Value,
        location: &LazyLocation,
    ) -> Result<(), jsonschema::ValidationError<'i>> {
        if self.is_valid(instance) {
            return Ok(());
        }
        Err(jsonschema::ValidationError::custom(
            self.path.clone(),
            location.into(),
            instance,
            "property rejected by ECMAScript schema constraints",
        ))
    }
}
impl PropertyKeyword {
    fn valid_property(&self, context: &Context, name: &str, value: &Value) -> bool {
        let matches: Vec<_> = self
            .patterns
            .iter()
            .filter(|(regex, _)| regex.find(name).is_some())
            .collect();
        if let Some(additional) = &self.additional {
            if let Some(path) = additional.declared.get(name) {
                // jsonschema delegates declared-property checks to additionalProperties
                // when its value is false or a schema, so the override must retain them.
                return matches!(additional.rule, AdditionalRule::Allow)
                    || context
                        .child(path)
                        .is_some_and(|validator| validator.is_valid(value));
            }
            if !matches.is_empty() {
                return true;
            }
            return match &additional.rule {
                AdditionalRule::Allow => true,
                AdditionalRule::Deny => false,
                AdditionalRule::Schema(path) => context
                    .child(path)
                    .is_some_and(|validator| validator.is_valid(value)),
            };
        }
        matches.iter().all(|(_, path)| {
            context
                .child(path)
                .is_some_and(|validator| validator.is_valid(value))
        })
    }
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
            patterns.push((
                regex,
                pointer(&pointer(location, "patternProperties"), pattern),
            ));
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
