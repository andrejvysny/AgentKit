//! Identifier newtypes, the workspace generation and the SHA-256 digest.
//!
//! Ids are opaque text. Production ids come from the kernel [`Runtime`]
//! identity port (UUID v7 under `SystemRuntime`); the types do not parse the
//! text, so a test runtime's `manual-N` ids work unchanged.

use std::fmt;

use agentkit_kernel::{Runtime, sha256_hex};
use serde::{Deserialize, Serialize};

macro_rules! text_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Wrap an id that already exists (read from a row or a payload).
            pub fn new(id: impl Into<String>) -> Self {
                Self(id.into())
            }

            /// The id's text form.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

text_id!(
    /// Identity of a workspace record. Minted once, at first use of its key.
    WorkspaceId
);
text_id!(
    /// Identity of one mutating operation: the `execution_id` the durable
    /// execution ledger granted at admission. The sandbox never derives its
    /// own (INV-2); it only wraps the granted id.
    OperationId
);
text_id!(
    /// An agent allowed to use a workspace, as the host names it.
    AgentId
);
text_id!(
    /// Identity of a network grant.
    GrantId
);
text_id!(
    /// Identity of a pending network request.
    RequestId
);

impl WorkspaceId {
    /// Mint a fresh workspace id through the runtime identity port.
    pub fn mint(runtime: &dyn Runtime) -> Self {
        Self(runtime.id())
    }
}

impl OperationId {
    /// Wrap the `execution_id` the durable ledger granted for this call.
    pub fn from_execution_id(execution_id: impl Into<String>) -> Self {
        Self(execution_id.into())
    }
}

impl GrantId {
    /// Mint a fresh grant id through the runtime identity port.
    pub fn mint(runtime: &dyn Runtime) -> Self {
        Self(runtime.id())
    }
}

impl RequestId {
    /// Mint a fresh request id through the runtime identity port.
    pub fn mint(runtime: &dyn Runtime) -> Self {
        Self(runtime.id())
    }
}

/// Machine generation of a workspace. It increases each time a machine is
/// started after a proven stop; every mutating operation carries it and a
/// stale one is refused (INV-5, INV-8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Generation(pub u64);

impl Generation {
    /// The generation a new workspace starts at.
    pub const FIRST: Generation = Generation(1);

    /// The generation after this one, or `None` on overflow.
    pub fn next(self) -> Option<Generation> {
        self.0.checked_add(1).map(Generation)
    }
}

impl fmt::Display for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A SHA-256 digest as 64 lowercase hex characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Sha256(String);

/// Why text is not a SHA-256 hex digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Sha256Error {
    /// The text is not 64 characters long.
    #[error("sha-256 digest must be 64 hex characters, got {0}")]
    Length(usize),
    /// The text has a character outside `0-9a-f`.
    #[error("sha-256 digest must be lowercase hex")]
    NotLowercaseHex,
}

impl Sha256 {
    /// Validate a hex digest.
    pub fn parse(text: &str) -> Result<Self, Sha256Error> {
        if text.len() != 64 {
            return Err(Sha256Error::Length(text.len()));
        }
        if !text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Sha256Error::NotLowercaseHex);
        }
        Ok(Self(text.to_owned()))
    }

    /// The digest of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        Self(sha256_hex(bytes))
    }

    /// The hex text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Sha256 {
    type Error = Sha256Error;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<Sha256> for String {
    fn from(digest: Sha256) -> Self {
        digest.0
    }
}

impl fmt::Display for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed;
    impl Runtime for Fixed {
        fn now_ms(&self) -> i64 {
            0
        }
        fn id(&self) -> String {
            "manual-1".into()
        }
    }

    #[test]
    fn ids_mint_through_the_runtime_and_serialize_as_text() {
        let id = WorkspaceId::mint(&Fixed);
        assert_eq!(id.as_str(), "manual-1");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"manual-1\"");
    }

    #[test]
    fn generation_advances_and_stops_at_overflow() {
        assert_eq!(Generation::FIRST.next(), Some(Generation(2)));
        assert_eq!(Generation(u64::MAX).next(), None);
    }

    #[test]
    fn sha256_validates_and_hashes() {
        let digest = Sha256::of(b"abc");
        assert_eq!(
            digest.as_str(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(Sha256::parse(digest.as_str()), Ok(digest.clone()));
        assert_eq!(Sha256::parse("ab"), Err(Sha256Error::Length(2)));
        let upper = digest.as_str().to_uppercase();
        assert_eq!(Sha256::parse(&upper), Err(Sha256Error::NotLowercaseHex));
        assert!(serde_json::from_str::<Sha256>("\"zz\"").is_err());
    }
}
