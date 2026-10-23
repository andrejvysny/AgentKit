use jsonschema::{Draft, Retrieve, Uri};
use serde_json::Value;
use std::{collections::BTreeMap, sync::OnceLock};

#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("unknown contract schema: {0}")]
    UnknownSchema(String),
    #[error("schema validation failed: {0}")]
    Invalid(String),
    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
}

struct NoRetrieval;

impl Retrieve for NoRetrieval {
    fn retrieve(
        &self,
        uri: &Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err(format!("external schema retrieval disabled: {uri}").into())
    }
}

pub fn schemas() -> &'static BTreeMap<String, Value> {
    static SCHEMAS: OnceLock<BTreeMap<String, Value>> = OnceLock::new();
    SCHEMAS.get_or_init(|| {
        serde_json::from_str(include_str!("schema_bundle.json")).expect("embedded schemas are JSON")
    })
}

/// Draft-07 validation. Explicit retriever denies both network and filesystem access.
pub fn compile_schema(schema: &Value) -> Result<crate::SchemaValidator, ValidationError> {
    crate::properties_validation::compile(schema, false)
}

fn validators() -> &'static BTreeMap<String, Result<crate::SchemaValidator, String>> {
    static VALIDATORS: OnceLock<BTreeMap<String, Result<crate::SchemaValidator, String>>> =
        OnceLock::new();
    VALIDATORS.get_or_init(|| {
        schemas()
            .iter()
            .map(|(name, schema)| {
                (
                    name.clone(),
                    compile_schema(schema).map_err(|error| error.to_string()),
                )
            })
            .collect()
    })
}

pub fn validate_schema(name: &str, value: &Value) -> Result<(), ValidationError> {
    let validator = validators()
        .get(name)
        .ok_or_else(|| ValidationError::UnknownSchema(name.into()))?
        .as_ref()
        .map_err(|error| ValidationError::Invalid(error.clone()))?;
    validator
        .validate(value)
        .map_err(|error| ValidationError::Invalid(error.to_string()))
}

pub fn validate_request(
    operation: crate::RestOperation,
    body: &Value,
) -> Result<(), ValidationError> {
    match operation.request_schema() {
        Some(name) => validate_schema(name, body),
        None if body.is_null() => Ok(()),
        None => Err(ValidationError::Invalid(
            "operation has no request body".into(),
        )),
    }
}

pub fn deserialize_validated<T: crate::JsonDeserialize>(
    name: &str,
    value: Value,
) -> Result<T, ValidationError> {
    validate_schema(name, &value)?;
    Ok(T::from_json(value)?)
}

pub(crate) fn schema_options() -> jsonschema::ValidationOptions {
    crate::numeric_validation::numeric_options(
        jsonschema::options()
            .with_draft(Draft::Draft7)
            .with_retriever(NoRetrieval)
            .with_keyword("pattern", crate::regexp::pattern_keyword),
        true,
    )
}
