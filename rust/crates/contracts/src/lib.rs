//! Provider-neutral wire contracts, generated from AgentKit's authoritative TypeBox schemas.
//! Unknown fields are retained where the TypeScript schema permits them.

mod ajv;
mod event;
mod generated;
mod idempotency;
mod javascript;
mod json_deserialize;
mod numeric_validation;
mod presence;
mod properties_validation;
mod regexp;
mod routes;
mod schema_mirror;
mod validation;

pub use ajv::*;
pub use event::*;
pub use generated::*;
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

/// Host-supplied wall time. Runtime code must not bypass this port.
pub trait Clock: Send + Sync {
    fn now_millis(&self) -> i64;
    fn now_iso(&self) -> String;
}

/// Host-supplied identity generation, with distinct namespaces for each record.
pub trait IdGenerator: Send + Sync {
    fn task_id(&self) -> String;
    fn attempt_id(&self) -> String;
    fn event_id(&self) -> String;
    fn proposal_id(&self) -> String;
    fn operation_id(&self) -> String;
    fn message_id(&self) -> String;
    fn chat_id(&self) -> String;
}
