use agentkit_contracts::{compile_schema, compile_tool_schema, schemas};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Deserialize)]
struct Corpus {
    version: u32,
    schemas: BTreeMap<String, Value>,
    vectors: Vec<Vector>,
    #[serde(rename = "compatibilityExceptions")]
    compatibility_exceptions: Vec<Value>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Vector {
    id: String,
    profile: String,
    schema: String,
    instance: Value,
    input_json: Option<String>,
    schema_json: Option<String>,
    known_divergence: Option<String>,
    expected: Outcome,
}
#[derive(Debug, Deserialize, PartialEq, Eq)]
struct Outcome {
    decoded: bool,
    compiled: bool,
    valid: bool,
}

fn corpus() -> Corpus {
    serde_json::from_str(include_str!("fixtures/schema-validation.json")).unwrap()
}

fn evaluate(corpus: &Corpus, vector: &Vector) -> Outcome {
    let instance = match vector
        .input_json
        .as_deref()
        .map(agentkit_contracts::parse_javascript_json)
        .transpose()
    {
        Ok(instance) => instance.unwrap_or_else(|| vector.instance.clone()),
        Err(_) => {
            return Outcome {
                decoded: false,
                compiled: false,
                valid: false,
            };
        }
    };
    let raw_schema = vector
        .schema_json
        .as_ref()
        .map(|text| agentkit_contracts::parse_javascript_json(text).unwrap());
    let schema = if let Some(schema) = raw_schema.as_ref() {
        schema
    } else if vector.profile == "contract" {
        &schemas()[&vector.schema]
    } else {
        &corpus.schemas[&vector.schema]
    };
    let compiled = if vector.profile == "contract" {
        compile_schema(schema)
    } else {
        compile_tool_schema(schema)
    };
    match compiled {
        Ok(validator) => Outcome {
            decoded: true,
            compiled: true,
            valid: validator.is_valid(&instance),
        },
        Err(_) => Outcome {
            decoded: true,
            compiled: false,
            valid: false,
        },
    }
}

#[test]
fn draft07_acceptance_matches_authoritative_ajv_vectors() {
    let corpus = corpus();
    assert_eq!(corpus.version, 1);
    assert_eq!(corpus.vectors.len(), 221);
    let mut differences = Vec::new();
    for vector in &corpus.vectors {
        if vector.known_divergence.is_some() {
            continue;
        }
        let actual = evaluate(&corpus, vector);
        if actual != vector.expected {
            differences.push(format!(
                "{}: Rust {actual:?}; TS {:?}",
                vector.id, vector.expected
            ));
        }
    }
    assert!(
        differences.is_empty(),
        "{} schema differences:\n{}",
        differences.len(),
        differences.join("\n")
    );
}

#[test]
fn lone_surrogate_rejection_is_an_explicit_approved_native_exception() {
    let corpus = corpus();
    assert_eq!(
        corpus.compatibility_exceptions[0]["status"],
        "user-approved"
    );
    let exceptions: Vec<_> = corpus
        .vectors
        .iter()
        .filter(|vector| vector.known_divergence.as_deref() == Some("rust-rejects-lone-surrogate"))
        .collect();
    assert_eq!(exceptions.len(), 2);
    for vector in exceptions {
        assert_eq!(
            vector.known_divergence.as_deref(),
            Some("rust-rejects-lone-surrogate")
        );
        assert_eq!(
            vector.expected,
            Outcome {
                decoded: true,
                compiled: true,
                valid: true
            }
        );
        assert_eq!(
            evaluate(&corpus, vector),
            Outcome {
                decoded: false,
                compiled: false,
                valid: false
            }
        );
    }
}

#[test]
fn numeric_overflow_preserves_javascript_number_callbacks_and_stringification() {
    let corpus = corpus();
    let vectors: Vec<_> = corpus
        .vectors
        .iter()
        .filter(|vector| vector.id.contains("number-overflow"))
        .collect();
    assert_eq!(vectors.len(), 4);
    for vector in vectors {
        assert_eq!(evaluate(&corpus, vector), vector.expected);
        let value =
            agentkit_contracts::parse_javascript_json(vector.input_json.as_ref().unwrap()).unwrap();
        let number = if vector.profile == "tool" {
            &value
        } else {
            &value["priority"]
        };
        assert!(number.is_number());
        assert!(
            agentkit_contracts::javascript_number(number)
                .unwrap()
                .is_infinite()
        );
        let output = agentkit_contracts::javascript_json_value(&value);
        assert!(
            if vector.profile == "tool" {
                &output
            } else {
                &output["priority"]
            }
            .is_null()
        );
    }
}
