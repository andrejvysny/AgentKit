//! The per-container channel token the launcher answers in `ensured` and
//! the guest must repeat in `hello`. It is a secret: its `Debug` is
//! redacted, no error or message carries it, and it is compared in
//! constant time.

/// Longest token the guest accepts in its environment.
const TOKEN_MAX_BYTES: usize = 256;

/// A channel token from the launcher.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ChannelToken(String);

impl ChannelToken {
    /// A token the guest can carry: 1 to 256 printable ASCII characters
    /// (the supervisor's own rule for `AGENTKIT_CHANNEL_TOKEN`).
    pub(crate) fn parse(token: String) -> Option<Self> {
        let usable = (1..=TOKEN_MAX_BYTES).contains(&token.len())
            && token.bytes().all(|b| b.is_ascii_graphic());
        usable.then_some(Self(token))
    }

    /// Whether `answered` is this token. The time taken depends only on
    /// the lengths, never on where the two differ.
    pub(crate) fn matches(&self, answered: &str) -> bool {
        let (ours, theirs) = (self.0.as_bytes(), answered.as_bytes());
        if ours.len() != theirs.len() {
            return false;
        }
        let difference = ours
            .iter()
            .zip(theirs)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b));
        std::hint::black_box(difference) == 0
    }
}

impl std::fmt::Debug for ChannelToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChannelToken(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_printable_and_bounded() {
        assert!(ChannelToken::parse("0123456789abcdef".into()).is_some());
        assert!(ChannelToken::parse("x".repeat(256)).is_some());
        for bad in [
            String::new(),
            "x".repeat(257),
            "a b".into(),
            "a\n".into(),
            "é".into(),
        ] {
            assert!(ChannelToken::parse(bad.clone()).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn only_the_same_token_matches_and_debug_hides_it() {
        let token = ChannelToken::parse("s3cr3t-token".into()).unwrap();
        assert!(token.matches("s3cr3t-token"));
        for other in ["s3cr3t-tokeN", "s3cr3t-toke", "s3cr3t-token!", ""] {
            assert!(!token.matches(other), "{other}");
        }
        assert!(!format!("{token:?}").contains("s3cr3t"));
    }
}
