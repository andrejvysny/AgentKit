//! Provider-neutral wire contracts. This crate is the wire source of truth: the DTOs in
//! `dto/`, the route table in `routes.rs` and `schema_bundle.json` are hand-owned and
//! edited together (ADR 0022). Unknown fields are retained where the schema permits them.

mod ajv;
mod dto;
mod event;
mod idempotency;
mod javascript;
mod json_deserialize;
mod numeric_validation;
mod presence;
mod properties_validation;
mod regexp;
mod routes;
mod schema_mirror;
mod tool_schema_profile;
mod validation;

pub use ajv::*;
pub use dto::*;
pub use event::*;
pub use idempotency::*;
pub use javascript::*;
pub use json_deserialize::*;
pub use presence::*;
pub use properties_validation::SchemaValidator;
pub use routes::*;
pub use validation::*;

/// Define an exact numeric literal without widening it to an arbitrary integer.
#[doc(hidden)]
#[macro_export]
macro_rules! number_literal {
    ($name:ident, $value:literal) => {
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        pub struct $name;
        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(
                &self,
                serializer: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                serializer.serialize_u64($value)
            }
        }
        $crate::impl_json_deserialize!($name, |value: serde_json::Value| {
            if value.is_number() && $crate::javascript_number(&value) == Some($value as f64) {
                Ok(Self)
            } else {
                Err($crate::json_error(concat!(
                    "expected literal ",
                    stringify!($value)
                )))
            }
        });
    };
}

pub const MEDIA_TYPE_PATTERN: &str =
    "^[A-Za-z0-9][A-Za-z0-9!#$&^_.+-]*/[A-Za-z0-9][A-Za-z0-9!#$&^_.+-]*$";
pub const IMAGE_URL_PATTERN: &str = "^https?://";
pub const TOOL_NAME_PATTERN: &str = "^[a-zA-Z0-9_-]+$";
pub const KNOWN_PROVIDER_KINDS: &[&str] = &[
    "openai",
    "openrouter",
    "openai-compatible",
    "lmstudio",
    "omlx",
    "ollama",
];
