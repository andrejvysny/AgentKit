//! Narrowing of parsed JSON tool arguments. Mirrors
//! `packages/toolpacks-coding/src/args.ts`.

use serde_json::{Map, Value};

use crate::errors::CodingError;

fn invalid(message: impl Into<String>) -> CodingError {
    CodingError::new("invalid_arguments", message)
}

/// Tool arguments arrive as parsed JSON; narrow them without trusting them.
pub fn as_record(input: &Value) -> Result<&Map<String, Value>, CodingError> {
    match input {
        Value::Object(map) => Ok(map),
        _ => Err(invalid("arguments must be an object")),
    }
}

/// JS `.length` is UTF-16 code units; count them to keep messages identical
/// for the surrogate-pair boundary cases.
fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

pub fn req_string(
    input: &Map<String, Value>,
    key: &str,
    max_length: usize,
) -> Result<String, CodingError> {
    let Some(value) = input.get(key) else {
        return Err(invalid(format!("\"{key}\" must be a non-empty string")));
    };
    let Value::String(value) = value else {
        return Err(invalid(format!("\"{key}\" must be a non-empty string")));
    };
    if value.is_empty() {
        return Err(invalid(format!("\"{key}\" must be a non-empty string")));
    }
    if utf16_len(value) > max_length {
        return Err(invalid(format!(
            "\"{key}\" is longer than {max_length} characters"
        )));
    }
    Ok(value.clone())
}

pub fn opt_string(
    input: &Map<String, Value>,
    key: &str,
    max_length: usize,
) -> Result<Option<String>, CodingError> {
    // Parity: JSON `null` is not JS `undefined` — a present key must still be
    // a usable value, so only a missing key reads as absent.
    match input.get(key) {
        None => Ok(None),
        Some(_) => req_string(input, key, max_length).map(Some),
    }
}

pub fn opt_bool(input: &Map<String, Value>, key: &str) -> Result<Option<bool>, CodingError> {
    match input.get(key) {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(invalid(format!("\"{key}\" must be a boolean"))),
    }
}

pub fn opt_int(
    input: &Map<String, Value>,
    key: &str,
    min: i64,
    max: i64,
) -> Result<Option<i64>, CodingError> {
    let Some(value) = input.get(key) else {
        return Ok(None);
    };
    // JS `Number.isInteger` semantics: integral finite numbers only.
    let number = match value.as_f64() {
        Some(number) if number.is_finite() && number == number.trunc() => number,
        _ => {
            return Err(invalid(format!(
                "\"{key}\" must be an integer between {min} and {max}"
            )));
        }
    };
    let number = number as i64;
    if number < min || number > max {
        return Err(invalid(format!(
            "\"{key}\" must be an integer between {min} and {max}"
        )));
    }
    Ok(Some(number))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_non_object() {
        assert_eq!(
            as_record(&json!([1])).unwrap_err().code,
            "invalid_arguments"
        );
    }

    #[test]
    fn string_bounds() {
        let value = json!({ "a": "x" });
        let map = as_record(&value).unwrap();
        assert_eq!(req_string(map, "a", 4096).unwrap(), "x");
        assert!(req_string(map, "b", 4096).is_err());
        let big = "x".repeat(5);
        let value = json!({ "a": big });
        let map = as_record(&value).unwrap();
        let error = req_string(map, "a", 4).unwrap_err();
        assert_eq!(error.code, "invalid_arguments");
        assert!(error.message.contains("longer than 4 characters"));
    }

    #[test]
    fn int_bounds() {
        let value = json!({ "a": 5, "b": 1.5, "c": "3" });
        let map = as_record(&value).unwrap();
        assert_eq!(opt_int(map, "a", 0, 10).unwrap(), Some(5));
        assert_eq!(
            opt_int(map, "b", 0, 10).unwrap_err().code,
            "invalid_arguments"
        );
        assert_eq!(
            opt_int(map, "c", 0, 10).unwrap_err().code,
            "invalid_arguments"
        );
        assert_eq!(
            opt_int(map, "a", 6, 10).unwrap_err().code,
            "invalid_arguments"
        );
        assert_eq!(opt_int(map, "missing", 0, 10).unwrap(), None);
    }
}
