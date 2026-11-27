//! Validated workspace ids and generations, and the channel token. Nothing
//! builds a path from a request value that has not passed through here.

use agentkit_sandbox_proto::valid_workspace_id;

use crate::error::ServiceError;

/// A workspace id that passed [`valid_workspace_id`]: 1 to 64 bytes of
/// `a-z`, `0-9` and `-`, so it is one safe path component.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct WorkspaceId(String);

impl WorkspaceId {
    /// Check `id`.
    pub(crate) fn parse(id: &str) -> Result<Self, ServiceError> {
        if valid_workspace_id(id) {
            Ok(Self(id.to_owned()))
        } else {
            Err(ServiceError::Invalid(
                "workspace id must be 1 to 64 characters of a-z, 0-9 and -".into(),
            ))
        }
    }

    /// The id.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Check a requested generation: generations start at 1.
pub(crate) fn check_generation(generation: u64) -> Result<u64, ServiceError> {
    if generation == 0 {
        return Err(ServiceError::Invalid(
            "generation must be at least 1".into(),
        ));
    }
    Ok(generation)
}

/// A fresh channel token for one generation (a v4 UUID as 32 lowercase hex
/// characters, 122 random bits). It goes into the kernel command line and
/// the `ensured` reply only; never into the state file, a log or an error.
pub(crate) fn new_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_generations_and_tokens_are_checked() {
        assert!(WorkspaceId::parse("ws-1").is_ok());
        for bad in ["", "WS", "a/b", "..", &"a".repeat(65)] {
            assert!(WorkspaceId::parse(bad).is_err(), "{bad}");
        }
        assert!(check_generation(0).is_err());
        assert_eq!(check_generation(3).unwrap(), 3);
        let (a, b) = (new_token(), new_token());
        assert_ne!(a, b);
        assert!(a.len() == 32 && a.bytes().all(|c| c.is_ascii_hexdigit()));
    }
}
