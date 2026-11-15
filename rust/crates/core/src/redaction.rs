//! The one credential redactor (ADR 0034): the output gate and the store's
//! consumer-view event redaction both ask these functions, so a key one of
//! them masks the other masks too.
use serde_json::Value;

const MASK: &str = "***";
/// Normalized (lowercase alphanumerics) substrings that name credential
/// material, so `AWS_SECRET_ACCESS_KEY`, `x-api-key`, `private_key` and
/// `apiKey` all match.
const CREDENTIAL_TOKENS: [&str; 10] = [
    "secret",
    "token",
    "password",
    "passwd",
    "apikey",
    "privatekey",
    "accesskey",
    "authorization",
    "cookie",
    "credential",
];
/// Exact normalized key names that hold references, counts or limits rather
/// than credential values (`secretRefs`, `max_tokens`). An exact list, not a
/// suffix rule: `refresh_tokens` or `access_tokens` must still be masked.
const NOT_A_VALUE: [&str; 16] = [
    "secretref",
    "secretrefs",
    "maxtokens",
    "maxoutputtokens",
    "maxcompletiontokens",
    "inputtokens",
    "outputtokens",
    "totaltokens",
    "cachedtokens",
    "reasoningtokens",
    "prompttokens",
    "completiontokens",
    "tokencount",
    "tokenusage",
    "tokenlimit",
    "tokenbudget",
];

fn normalize(key: &str) -> String {
    key.chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// A key whose VALUE is credential material.
pub fn is_sensitive_key(key: &str) -> bool {
    let normalized = normalize(key);
    if NOT_A_VALUE.contains(&normalized.as_str()) {
        return false;
    }
    CREDENTIAL_TOKENS
        .iter()
        .any(|token| normalized.contains(token))
}

/// Maps that bag request headers or process environment: credential-shaped
/// bags whose every non-null value is redacted whole.
pub fn is_bag_key(key: &str) -> bool {
    let normalized = normalize(key);
    normalized == "env" || normalized == "headers"
}

/// Replace the value of every credential-named key, and every member of an
/// `env`/`headers` bag, with `"***"`. Null stays null (absence is honest).
pub fn redact_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if is_sensitive_key(key) {
                    if !entry.is_null() {
                        *entry = Value::String(MASK.into());
                    }
                } else if is_bag_key(key) {
                    if let Value::Object(bag) = entry {
                        for inner in bag.values_mut().filter(|inner| !inner.is_null()) {
                            *inner = Value::String(MASK.into());
                        }
                    } else {
                        redact_value(entry);
                    }
                } else {
                    redact_value(entry);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_value),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn token_bearing_plurals_stay_masked() {
        assert!(super::is_sensitive_key("refresh_tokens"));
        assert!(super::is_sensitive_key("accessTokens"));
        assert!(!super::is_sensitive_key("max_tokens"));
        assert!(!super::is_sensitive_key("secretRefs"));
    }

    use super::*;
    use serde_json::json;

    #[test]
    fn cloud_and_header_style_names_are_credentials() {
        for key in [
            "AWS_SECRET_ACCESS_KEY",
            "x-api-key",
            "private_key",
            "access_key",
            "apiKey",
            "Authorization",
            "Set-Cookie",
            "client_secret",
            "DB_PASSWORD",
        ] {
            assert!(is_sensitive_key(key), "{key}");
        }
        for key in ["secretRefs", "max_tokens", "total_tokens", "path", "status"] {
            assert!(!is_sensitive_key(key), "{key}");
        }
    }

    #[test]
    fn values_and_bags_are_masked() {
        let mut value = json!({
            "AWS_SECRET_ACCESS_KEY": "S1",
            "env": {"HOME": "/h", "N": null},
            "nested": [{"x-api-key": "S2", "ok": 1}],
            "secretRefs": ["vault://a"]
        });
        redact_value(&mut value);
        assert_eq!(value["AWS_SECRET_ACCESS_KEY"], json!("***"));
        assert_eq!(value["env"]["HOME"], json!("***"));
        assert_eq!(value["env"]["N"], Value::Null);
        assert_eq!(value["nested"][0]["x-api-key"], json!("***"));
        assert_eq!(value["nested"][0]["ok"], json!(1));
        assert_eq!(value["secretRefs"][0], json!("vault://a"));
    }
}
