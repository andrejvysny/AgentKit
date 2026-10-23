//! Tool-schema compatibility with the repository's Ajv 8 + ajv-formats profile.
use crate::validation::{ValidationError, schema_options};
use jsonschema::{
    Keyword, Validator,
    paths::{LazyLocation, Location},
};
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, sync::OnceLock};

/// Unlike wire DTO validation, tool schemas enable the actual Ajv-format vocabulary.
// jsonschema fixes keyword callbacks to its large borrowed ValidationError type.
#[allow(clippy::result_large_err)]
pub fn compile_tool_schema(schema: &Value) -> Result<crate::SchemaValidator, ValidationError> {
    crate::tool_schema_profile::check(schema)?;
    crate::properties_validation::compile(schema, true)
}

// jsonschema fixes keyword callbacks to its large borrowed ValidationError type.
#[allow(clippy::result_large_err)]
pub(crate) fn profile_options(marker: String, tools: bool) -> jsonschema::ValidationOptions {
    let mut options = crate::numeric_validation::numeric_options(schema_options(), !tools);
    if tools {
        options = options
            .should_validate_formats(true)
            .with_keyword("format", format_keyword);
    }
    for name in [
        "minItems",
        "maxItems",
        "minLength",
        "maxLength",
        "minProperties",
        "maxProperties",
    ] {
        let marker = marker.clone();
        options = options.with_keyword(name, move |parent, value, path| {
            crate::numeric_validation::size_keyword(parent, value, path, name, &marker)
        });
    }
    options.with_keyword("multipleOf", move |parent, value, path| {
        let divisor = if parent
            .get(&marker)
            .and_then(|value| value.get("multipleOf"))
            .and_then(Value::as_bool)
            == Some(true)
        {
            Some(f64::INFINITY)
        } else {
            crate::javascript_number(value)
        };
        multiple_factory(value, path, divisor)
    })
}

enum Rule {
    Format {
        name: String,
        validator: FormatValidator,
    },
    MultipleOf(f64),
}
struct AjvKeyword {
    path: Location,
    rule: Rule,
}

impl Keyword for AjvKeyword {
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
            "value rejected by AgentKit's Ajv compatibility profile",
        ))
    }
    fn is_valid(&self, instance: &Value) -> bool {
        match &self.rule {
            Rule::MultipleOf(divisor) => crate::javascript_number(instance).is_none_or(|number| {
                let quotient = number / divisor;
                quotient.is_finite() && javascript_parse_int(quotient) == Some(quotient)
            }),
            Rule::Format { name, validator } => format_valid(name, validator, instance),
        }
    }
}

// jsonschema requires its borrowed ValidationError in keyword factory signatures.
#[allow(clippy::result_large_err)]
pub(crate) fn multiple_keyword<'a>(
    _parent: &'a Map<String, Value>,
    value: &'a Value,
    path: Location,
) -> Result<Box<dyn Keyword>, jsonschema::ValidationError<'a>> {
    multiple_factory(value, path, crate::javascript_number(value))
}

// jsonschema requires its borrowed ValidationError in keyword factory signatures.
#[allow(clippy::result_large_err)]
fn multiple_factory<'a>(
    value: &'a Value,
    path: Location,
    divisor: Option<f64>,
) -> Result<Box<dyn Keyword>, jsonschema::ValidationError<'a>> {
    let divisor = divisor.filter(|number| *number > 0.0).ok_or_else(|| {
        jsonschema::ValidationError::custom(
            path.clone(),
            Location::new(),
            value,
            "multipleOf must be positive",
        )
    })?;
    Ok(Box::new(AjvKeyword {
        path,
        rule: Rule::MultipleOf(divisor),
    }))
}

// jsonschema requires its borrowed ValidationError in keyword factory signatures.
#[allow(clippy::result_large_err)]
fn format_keyword<'a>(
    _parent: &'a Map<String, Value>,
    value: &'a Value,
    path: Location,
) -> Result<Box<dyn Keyword>, jsonschema::ValidationError<'a>> {
    let name = value.as_str().ok_or_else(|| {
        jsonschema::ValidationError::custom(
            path.clone(),
            Location::new(),
            value,
            "format must be a string",
        )
    })?;
    let validator = format_validator(name).map_err(|error| {
        jsonschema::ValidationError::custom(path.clone(), Location::new(), value, error)
    })?;
    Ok(Box::new(AjvKeyword {
        path,
        rule: Rule::Format {
            name: name.into(),
            validator,
        },
    }))
}

fn patterns() -> &'static BTreeMap<String, Value> {
    static PATTERNS: OnceLock<BTreeMap<String, Value>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        serde_json::from_str(include_str!("ajv_format_patterns.json"))
            .expect("Ajv pattern snapshot")
    })
}

enum FormatValidator {
    Pattern {
        regex: regress::Regex,
        unicode: bool,
    },
    Schema(Validator),
}
impl FormatValidator {
    fn is_valid(&self, instance: &Value) -> bool {
        match self {
            Self::Schema(validator) => validator.is_valid(instance),
            Self::Pattern { regex, unicode } => instance.as_str().is_none_or(|text| {
                if *unicode {
                    regex.find(text).is_some()
                } else {
                    regex
                        .find_from_ucs2(&text.encode_utf16().collect::<Vec<_>>(), 0)
                        .next()
                        .is_some()
                }
            }),
        }
    }
}

fn format_validator(name: &str) -> Result<FormatValidator, String> {
    if let Some(pattern) = patterns().get(name) {
        let flags = pattern["flags"].as_str().unwrap_or("");
        let regex =
            regress::Regex::with_flags(pattern["source"].as_str().expect("pattern source"), flags)
                .map_err(|error| error.to_string())?;
        return Ok(FormatValidator::Pattern {
            regex,
            unicode: flags.contains('u'),
        });
    }
    if !matches!(
        name,
        "date"
            | "date-time"
            | "time"
            | "iso-time"
            | "iso-date-time"
            | "uri"
            | "regex"
            | "int32"
            | "int64"
            | "float"
            | "double"
            | "password"
            | "binary"
    ) {
        return Err(format!("Unknown tool schema format: {name}"));
    }
    let schema = json!({"format": if name == "iso-time" { "time" } else if name == "iso-date-time" { "date-time" } else {name}});
    schema_options()
        .should_validate_formats(true)
        .build(&schema)
        .map(FormatValidator::Schema)
        .map_err(|error| error.to_string())
}

fn format_valid(name: &str, validator: &FormatValidator, instance: &Value) -> bool {
    if matches!(name, "int32" | "int64") {
        return crate::javascript_number(instance).is_none_or(|number| {
            number.is_finite()
                && number.fract() == 0.0
                && (name != "int32" || (-2_147_483_648.0..=2_147_483_647.0).contains(&number))
        });
    }
    let Some(text) = instance.as_str() else {
        return true;
    };
    if matches!(name, "date-time" | "iso-date-time") {
        let text = normalize_datetime(text);
        return validator.is_valid(&Value::String(if name == "iso-date-time" {
            normalize_timezone(&text, true)
        } else {
            normalize_timezone(&text, false)
        }));
    }
    if matches!(name, "iso-time" | "time") {
        return validator.is_valid(&Value::String(normalize_timezone(text, name == "iso-time")));
    }
    validator.is_valid(instance)
}

fn normalize_datetime(text: &str) -> String {
    if let Some(tail) = text.get(10..)
        && let Some(separator) = tail.chars().next()
        && javascript_whitespace(separator)
    {
        return format!("{}T{}", &text[..10], &tail[separator.len_utf8()..]);
    }
    text.into()
}

fn normalize_timezone(text: &str, optional: bool) -> String {
    let time = text.rsplit_once(['T', 't']).map_or(text, |(_, time)| time);
    if let Some(index) = time.rfind(['+', '-']) {
        let offset = &time[index..];
        if offset.len() == 3 && offset[1..].bytes().all(|byte| byte.is_ascii_digit()) {
            return format!("{text}:00");
        }
        if offset.len() == 5 && offset[1..].bytes().all(|byte| byte.is_ascii_digit()) {
            return format!("{}:{}", &text[..text.len() - 2], &text[text.len() - 2..]);
        }
    } else if optional && !time.ends_with(['Z', 'z']) {
        return format!("{text}Z");
    }
    text.into()
}

// Ajv uses parseInt on the quotient string, including JavaScript exponent thresholds.
fn javascript_parse_int(number: f64) -> Option<f64> {
    let mut buffer = ryu_js::Buffer::new();
    let text = buffer.format(number);
    let end = text
        .char_indices()
        .find(|(index, ch)| !ch.is_ascii_digit() && !(*index == 0 && *ch == '-'))
        .map_or(text.len(), |(index, _)| index);
    text[..end].parse().ok()
}

fn javascript_whitespace(ch: char) -> bool {
    matches!(ch, '\u{0009}'..='\u{000d}' | ' ' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}
