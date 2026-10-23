//! Avoid jsonschema's finite-number assumptions for JavaScript overflow values.
use crate::javascript_number;
use jsonschema::{
    Keyword, ValidationOptions,
    paths::{LazyLocation, Location},
};
use serde_json::{Map, Value};
use std::collections::HashSet;

enum Rule {
    Type {
        names: Vec<String>,
        strict_numbers: bool,
    },
    Bound {
        name: &'static str,
        value: f64,
    },
    Enum(Vec<Value>),
    Unique(bool),
    Size {
        name: &'static str,
        bound: f64,
    },
}
struct NumericKeyword {
    path: Location,
    rule: Rule,
}
impl Keyword for NumericKeyword {
    fn is_valid(&self, instance: &Value) -> bool {
        match &self.rule {
            Rule::Size { name, bound } => size_valid(name, *bound, instance),
            Rule::Enum(values) => values.iter().any(|value| javascript_equal(value, instance)),
            Rule::Unique(enabled) => {
                !enabled
                    || instance.as_array().is_none_or(|items| {
                        let mut seen = HashSet::with_capacity(items.len());
                        items.iter().all(|item| seen.insert(equality_key(item)))
                    })
            }
            Rule::Type {
                names,
                strict_numbers,
            } => names
                .iter()
                .any(|name| type_valid(name, *strict_numbers, instance)),
            Rule::Bound { name, value } => {
                javascript_number(instance).is_none_or(|number| match *name {
                    "minimum" => number >= *value,
                    "maximum" => number <= *value,
                    "exclusiveMinimum" => number > *value,
                    "exclusiveMaximum" => number < *value,
                    _ => false,
                })
            }
        }
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
            "value rejected by JavaScript numeric schema semantics",
        ))
    }
}

fn size_valid(name: &str, bound: f64, instance: &Value) -> bool {
    let length = match name {
        "minItems" | "maxItems" => instance.as_array().map(Vec::len),
        "minProperties" | "maxProperties" => instance.as_object().map(Map::len),
        _ => instance.as_str().map(|text| text.chars().count()),
    };
    length.is_none_or(|length| {
        if name.starts_with("min") {
            length as f64 >= bound
        } else {
            length as f64 <= bound
        }
    })
}

fn type_valid(name: &str, strict: bool, instance: &Value) -> bool {
    match name {
        "null" => instance.is_null(),
        "boolean" => instance.is_boolean(),
        "string" => instance.is_string(),
        "array" => instance.is_array(),
        "object" => instance.is_object(),
        "number" => javascript_number(instance).is_some_and(|n| !strict || n.is_finite()),
        "integer" => javascript_number(instance)
            .is_some_and(|n| (n.fract() == 0.0 || n.is_infinite()) && (!strict || n.is_finite())),
        _ => false,
    }
}

// jsonschema requires this borrowed error type for keyword factories.
#[allow(clippy::result_large_err)]
pub(crate) fn size_keyword<'a>(
    parent: &'a Map<String, Value>,
    value: &'a Value,
    path: Location,
    name: &'static str,
    marker: &str,
) -> Result<Box<dyn Keyword>, jsonschema::ValidationError<'a>> {
    let bound = if parent
        .get(marker)
        .and_then(|value| value.get(name))
        .and_then(Value::as_bool)
        == Some(true)
    {
        Some(f64::INFINITY)
    } else {
        javascript_number(value)
    };
    let bound = bound
        .filter(|number| *number >= 0.0 && (number.is_infinite() || number.fract() == 0.0))
        .ok_or_else(|| {
            jsonschema::ValidationError::custom(
                path.clone(),
                Location::new(),
                value,
                "size bound must be a nonnegative integer",
            )
        })?;
    Ok(Box::new(NumericKeyword {
        path,
        rule: Rule::Size { name, bound },
    }))
}

fn equality_key(value: &Value) -> String {
    match value {
        Value::Number(_) => {
            let number = javascript_number(value).expect("JSON number");
            if number.is_infinite() {
                return if number.is_sign_negative() {
                    "-Infinity"
                } else {
                    "Infinity"
                }
                .into();
            }
            ryu_js::Buffer::new().format(number).into()
        }
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(equality_key).collect::<Vec<_>>().join(",")
        ),
        Value::Object(object) => {
            let mut entries: Vec<_> = object.iter().collect();
            entries.sort_by(|(a, _), (b, _)| a.encode_utf16().cmp(b.encode_utf16()));
            format!(
                "{{{}}}",
                entries
                    .into_iter()
                    .map(|(key, value)| format!(
                        "{}:{}",
                        serde_json::to_string(key).expect("JSON key"),
                        equality_key(value)
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        _ => value.to_string(),
    }
}

// jsonschema's callback API fixes the large borrowed ValidationError return type.
#[allow(clippy::result_large_err)]
pub(crate) fn numeric_options(
    options: ValidationOptions,
    strict_numbers: bool,
) -> ValidationOptions {
    options
        .with_keyword("multipleOf", crate::ajv::multiple_keyword)
        .with_keyword("type", move |parent, value, path| {
            type_keyword(parent, value, path, strict_numbers)
        })
        .with_keyword("minimum", move |_, value, path| {
            bound_keyword(value, path, "minimum", strict_numbers)
        })
        .with_keyword("maximum", move |_, value, path| {
            bound_keyword(value, path, "maximum", strict_numbers)
        })
        .with_keyword("exclusiveMinimum", move |_, value, path| {
            bound_keyword(value, path, "exclusiveMinimum", strict_numbers)
        })
        .with_keyword("exclusiveMaximum", move |_, value, path| {
            bound_keyword(value, path, "exclusiveMaximum", strict_numbers)
        })
        .with_keyword("const", |_, value, path| {
            Ok(Box::new(NumericKeyword {
                path,
                rule: Rule::Enum(vec![value.clone()]),
            }) as Box<dyn Keyword>)
        })
        .with_keyword("enum", |_, value, path| {
            equality_keyword(value, path, false)
        })
        .with_keyword("uniqueItems", |_, value, path| {
            equality_keyword(value, path, true)
        })
}

fn javascript_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(_), Value::Number(_)) => javascript_number(left) == javascript_number(right),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| javascript_equal(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter().all(|(key, value)| {
                    b.get(key)
                        .is_some_and(|other| javascript_equal(value, other))
                })
        }
        _ => left == right,
    }
}

// jsonschema requires this borrowed error type for keyword factories.
#[allow(clippy::result_large_err)]
fn equality_keyword<'a>(
    value: &'a Value,
    path: Location,
    unique: bool,
) -> Result<Box<dyn Keyword>, jsonschema::ValidationError<'a>> {
    let rule = if unique {
        value.as_bool().map(Rule::Unique)
    } else {
        value
            .as_array()
            .filter(|items| !items.is_empty())
            .cloned()
            .map(Rule::Enum)
    };
    let rule = rule.ok_or_else(|| {
        jsonschema::ValidationError::custom(
            path.clone(),
            Location::new(),
            value,
            "invalid equality keyword",
        )
    })?;
    Ok(Box::new(NumericKeyword { path, rule }))
}

// jsonschema requires this borrowed error type for keyword factories.
#[allow(clippy::result_large_err)]
fn type_keyword<'a>(
    _: &'a Map<String, Value>,
    value: &'a Value,
    path: Location,
    strict_numbers: bool,
) -> Result<Box<dyn Keyword>, jsonschema::ValidationError<'a>> {
    let names: Option<Vec<String>> = if let Some(name) = value.as_str() {
        Some(vec![name.into()])
    } else {
        value.as_array().and_then(|items| {
            items
                .iter()
                .map(|item| item.as_str().map(String::from))
                .collect()
        })
    };
    let names = names
        .filter(|names| {
            !names.is_empty()
                && names.iter().all(|name| {
                    matches!(
                        name.as_str(),
                        "null" | "boolean" | "string" | "array" | "object" | "number" | "integer"
                    )
                })
        })
        .ok_or_else(|| {
            jsonschema::ValidationError::custom(
                path.clone(),
                Location::new(),
                value,
                "invalid schema type",
            )
        })?;
    Ok(Box::new(NumericKeyword {
        path,
        rule: Rule::Type {
            names,
            strict_numbers,
        },
    }))
}

// jsonschema requires this borrowed error type for keyword factories.
#[allow(clippy::result_large_err)]
fn bound_keyword<'a>(
    value: &'a Value,
    path: Location,
    name: &'static str,
    strict_numbers: bool,
) -> Result<Box<dyn Keyword>, jsonschema::ValidationError<'a>> {
    let number = javascript_number(value)
        .filter(|n| !strict_numbers || n.is_finite())
        .ok_or_else(|| {
            jsonschema::ValidationError::custom(
                path.clone(),
                Location::new(),
                value,
                "invalid numeric bound",
            )
        })?;
    Ok(Box::new(NumericKeyword {
        path,
        rule: Rule::Bound {
            name,
            value: number,
        },
    }))
}
