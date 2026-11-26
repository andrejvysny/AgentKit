//! The channel token: a fresh random secret for each created container,
//! placed in its environment (`AGENTKIT_CHANNEL_TOKEN`) and answered in
//! `ensured`. The guest repeats it in `hello`, so the adapter can tell its
//! guest from any other process that reaches the control socket first.
//!
//! The launcher stores no token: an `ensure` of the running generation
//! reads it back from the container's environment (the runtime's record,
//! which the container cannot change). It never appears in a log line or
//! an error message.

/// The variable the supervisor reads its token from.
pub(crate) const TOKEN_ENV: &str = "AGENTKIT_CHANNEL_TOKEN";
/// Length of a token: a v4 UUID as 32 lowercase hex characters.
const TOKEN_LEN: usize = 32;

/// A new token (122 random bits).
pub(crate) fn new_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// A token as the launcher makes them.
pub(crate) fn valid_token(token: &str) -> bool {
    token.len() == TOKEN_LEN
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The token of a container's environment: exactly one
/// `AGENTKIT_CHANNEL_TOKEN=` entry holding a valid token, else `None`.
pub(crate) fn token_of(env: &[String]) -> Option<String> {
    let prefix = format!("{TOKEN_ENV}=");
    let mut values = env.iter().filter_map(|entry| entry.strip_prefix(&prefix));
    match (values.next(), values.next()) {
        (Some(token), None) if valid_token(token) => Some(token.to_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_fresh_and_valid() {
        let (a, b) = (new_token(), new_token());
        assert!(valid_token(&a) && valid_token(&b));
        assert_ne!(a, b);
    }

    #[test]
    fn only_one_valid_entry_is_a_token() {
        let token = new_token();
        let entry = format!("{TOKEN_ENV}={token}");
        let other = "AGENTKIT_GENERATION=1".to_owned();
        assert_eq!(token_of(&[other.clone(), entry.clone()]), Some(token));
        assert_eq!(token_of(std::slice::from_ref(&other)), None);
        assert_eq!(token_of(&[entry.clone(), entry]), None, "two entries");
        for bad in ["", "ABCDEF", "0123", &"g".repeat(TOKEN_LEN)] {
            assert_eq!(token_of(&[format!("{TOKEN_ENV}={bad}")]), None, "{bad}");
        }
    }
}
