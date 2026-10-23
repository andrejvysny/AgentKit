//! Ajv's Unicode ECMAScript pattern semantics, rather than Rust regex syntax.
use jsonschema::{
    Keyword,
    paths::{LazyLocation, Location},
};
use serde_json::{Map, Value};

struct Pattern {
    path: Location,
    regex: regress::Regex,
}
impl Keyword for Pattern {
    fn is_valid(&self, instance: &Value) -> bool {
        instance
            .as_str()
            .is_none_or(|text| self.regex.find(text).is_some())
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
            "value does not match ECMAScript pattern",
        ))
    }
}

// jsonschema requires this borrowed error type for keyword factories.
#[allow(clippy::result_large_err)]
pub(crate) fn pattern_keyword<'a>(
    _: &'a Map<String, Value>,
    value: &'a Value,
    path: Location,
) -> Result<Box<dyn Keyword>, jsonschema::ValidationError<'a>> {
    let regex = value
        .as_str()
        .ok_or_else(|| "pattern must be a string".to_owned())
        .and_then(|pattern| {
            regress::Regex::with_flags(pattern, "u").map_err(|error| error.to_string())
        })
        .map_err(|error| {
            jsonschema::ValidationError::custom(path.clone(), Location::new(), value, error)
        })?;
    Ok(Box::new(Pattern { path, regex }))
}
