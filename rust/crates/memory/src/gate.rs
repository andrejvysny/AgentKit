//! The minimal write gate (spec, "The write gate"): the checks that run in
//! code before any record is inserted, whatever the model claims. This is
//! the part roadmap package 3.3 needs (owner-words containment, remember
//! intent, size, secrets);
//! package 3.4 extends it with duplicate, update and rate rules and the
//! sensitive-category downgrade. The model's judgment is never the check.

use crate::model::MAX_TEXT_CHARS;

/// A quote shorter than this proves nothing.
pub const MIN_QUOTE_CHARS: usize = 12;

/// Why a statement or a quote was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateRefusal {
    Empty,
    TooLong,
    Secret,
}

impl GateRefusal {
    pub fn message(self) -> String {
        match self {
            Self::Empty => "The text is empty.".to_owned(),
            Self::TooLong => {
                format!("The text is longer than {MAX_TEXT_CHARS} characters; state one or two sentences.")
            }
            Self::Secret => "The text looks like a secret (a key, token, password or private key) and is never stored.".to_owned(),
        }
    }
}

/// Why a quote did not verify against the owner's message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteCheck {
    Contained,
    TooShort,
    NotFound,
}

/// Size and secret screening of a candidate record text.
pub fn screen_text(text: &str) -> Result<(), GateRefusal> {
    if text.trim().is_empty() {
        return Err(GateRefusal::Empty);
    }
    if text.chars().count() > MAX_TEXT_CHARS {
        return Err(GateRefusal::TooLong);
    }
    if contains_secret(text) {
        return Err(GateRefusal::Secret);
    }
    Ok(())
}

/// W1 evidence: `text` verbatim in the owner's eligible typed text, after
/// whitespace, invisible-character, typographic and case normalization, and
/// at least [`MIN_QUOTE_CHARS`] long. Returns the matched span as the owner
/// typed it (normalized spacing and punctuation, original case); that span,
/// never anything the model chose, is the record's evidence quote.
pub fn owner_span(text: &str, eligible: &str) -> Result<String, QuoteCheck> {
    let needle: Vec<char> = normalize(text).chars().collect();
    if needle.len() < MIN_QUOTE_CHARS {
        return Err(QuoteCheck::TooShort);
    }
    let hay: Vec<char> = normalize(eligible).chars().collect();
    if hay.len() >= needle.len() {
        for start in 0..=hay.len() - needle.len() {
            let window = &hay[start..start + needle.len()];
            if window.iter().zip(&needle).all(|(a, b)| same_char(*a, *b)) {
                return Ok(window.iter().collect());
            }
        }
    }
    Err(QuoteCheck::NotFound)
}

fn same_char(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

/// Phrases that make a message an explicit request to remember, in their
/// folded form (lowercase, no diacritics): English and Slovak. This is the
/// one list; the model's own `owner_requested` flag is never consulted.
pub const REMEMBER_PHRASES: [&str; 9] = [
    "remember",
    "don't forget",
    "dont forget",
    "note that",
    "keep in mind",
    "zapamataj",
    "pamataj si",
    "nezabudni",
    "poznac si",
];

fn fold_char(ch: char) -> char {
    match ch {
        'á' | 'ä' | 'à' | 'â' | 'ã' | 'å' | 'ā' => 'a',
        'č' | 'ć' | 'ç' => 'c',
        'ď' => 'd',
        'é' | 'ě' | 'è' | 'ê' | 'ë' | 'ē' | 'ę' => 'e',
        'í' | 'ì' | 'î' | 'ï' => 'i',
        'ĺ' | 'ľ' | 'ł' => 'l',
        'ň' | 'ń' | 'ñ' => 'n',
        'ó' | 'ô' | 'ö' | 'ò' | 'õ' | 'ő' => 'o',
        'ŕ' | 'ř' => 'r',
        'š' | 'ś' => 's',
        'ť' => 't',
        'ú' | 'ü' | 'ù' | 'û' | 'ů' | 'ű' => 'u',
        'ý' | 'ÿ' => 'y',
        'ž' | 'ź' | 'ż' => 'z',
        other => other,
    }
}

/// Whether the owner's message asks, in so many words, to remember
/// something: a [`REMEMBER_PHRASES`] entry as whole words, ignoring case and
/// diacritics.
pub fn has_remember_request(owner_text: &str) -> bool {
    let folded: String = normalize(owner_text)
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|ch| !('\u{0300}'..='\u{036F}').contains(ch))
        .map(fold_char)
        .collect();
    REMEMBER_PHRASES.iter().any(|phrase| {
        folded.match_indices(phrase).any(|(at, _)| {
            let before = folded[..at].chars().next_back();
            let after = folded[at + phrase.len()..].chars().next();
            !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
        })
    })
}

/// Invisible format characters (zero-width, joiners, bidi controls, soft
/// hyphen, BOM): they hide or split words without showing.
pub fn is_invisible(ch: char) -> bool {
    matches!(ch,
        '\u{00AD}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{206F}' | '\u{FEFF}')
}

/// `text` without its invisible format characters.
pub fn strip_invisible(text: &str) -> String {
    text.chars().filter(|ch| !is_invisible(*ch)).collect()
}

/// The comparison form of a text: whitespace runs (including no-break and
/// other Unicode spaces) become one space, invisible format characters
/// vanish, and typographic quotes, dashes and the ellipsis fold to ASCII.
/// Case is kept: a quote is verbatim.
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.chars() {
        let folded: &str = match ch {
            c if is_invisible(c) => continue,
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{2032}' => "'",
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{2033}' => "\"",
            '\u{2010}'..='\u{2015}' | '\u{2212}' => "-",
            '\u{2026}' => "...",
            c if c.is_whitespace() => {
                pending_space = !out.is_empty();
                continue;
            }
            _ => "",
        };
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        if folded.is_empty() {
            out.push(ch);
        } else {
            out.push_str(folded);
        }
    }
    out
}

/// The comparison form for the duplicate check: normalized and lowercased.
pub fn normalized_key(text: &str) -> String {
    normalize(text).to_lowercase()
}

const SECRET_PREFIXES: [&str; 19] = [
    "sk-",
    "sk_",
    "sk-ant-",
    "ghp_",
    "gho_",
    "ghs_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxs-",
    "xoxe-",
    "hf_",
    "npm_",
    "shpat_",
    "shpss_",
    "dop_v1_",
    "aiza",
];
/// Labels whose value follows a `:` or `=` (or ` is `, ` je `).
const SECRET_LABELS: [&str; 19] = [
    "password",
    "passwd",
    "secret",
    "api_key",
    "api-key",
    "apikey",
    "api key",
    "access_token",
    "access token",
    "auth_token",
    "private_key",
    "private key",
    "secret key",
    "client_secret",
    "token",
    "pwd",
    "heslo",
    "pristupovy kluc",
    "prístupový kľúč",
];
/// Labels whose value may follow after plain spaces.
const SPACED_LABELS: [&str; 3] = ["bearer", "authorization", "heslo"];

fn is_run_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '+' | '/' | '=' | '.')
}

fn run_len(text: &str) -> usize {
    text.chars().take_while(|ch| is_run_char(*ch)).count()
}

/// A value after a credential label: everything up to the next space,
/// punctuation included.
fn value_len(text: &str) -> usize {
    text.chars().take_while(|ch| !ch.is_whitespace()).count()
}

/// Whether `text` holds credential material: a private key block, a token
/// with a well-known prefix, a JSON web token, an AWS access key id, or a
/// credential label followed by a long value. Deliberately blunt: a false
/// positive costs one refused sentence, a false negative stores a secret.
pub fn contains_secret(text: &str) -> bool {
    let text = strip_invisible(text);
    if text.contains("-----BEGIN") && text.to_uppercase().contains("PRIVATE KEY") {
        return true;
    }
    let lower = text.to_lowercase();
    for (at, _) in lower.char_indices() {
        let before_is_word = lower[..at]
            .chars()
            .next_back()
            .is_some_and(|ch| ch.is_alphanumeric() || ch == '_');
        if before_is_word {
            continue;
        }
        let rest = &lower[at..];
        if SECRET_PREFIXES
            .iter()
            .any(|prefix| rest.starts_with(prefix) && run_len(&rest[prefix.len()..]) >= 8)
        {
            return true;
        }
        if rest.starts_with("akia") && run_len(rest) >= 20 {
            return true;
        }
        if rest.starts_with("eyj")
            && run_len(rest) >= 30
            && rest[..run_len(rest)].matches('.').count() >= 2
        {
            return true;
        }
        if labelled_value(rest) {
            return true;
        }
    }
    false
}

/// `label` + separator + a value of at least 8 run characters.
fn labelled_value(rest: &str) -> bool {
    for label in SPACED_LABELS {
        if let Some(after) = rest.strip_prefix(label)
            && after.starts_with([' ', ':', '=', '\t'])
        {
            let value = after.trim_start_matches([' ', ':', '=', '\t', '"', '\'']);
            if value_len(value) >= 8 {
                return true;
            }
        }
    }
    for label in SECRET_LABELS {
        let Some(after) = rest.strip_prefix(label) else {
            continue;
        };
        // `password: hunter2hunter2`, `api_key="..."`, `secret = ...`.
        let trimmed = after.trim_start_matches([' ', '\t', '"', '\'']);
        if let Some(value) = trimmed.strip_prefix([':', '=']) {
            let value = value.trim_start_matches([' ', '\t', '"', '\'']);
            if value_len(value) >= 8 {
                return true;
            }
        }
        // `the password is hunter2hunter2`.
        if let Some(value) = trimmed
            .strip_prefix("is ")
            .or_else(|| trimmed.strip_prefix("je "))
        {
            let value = value.trim_start_matches([' ', '\t', '"', '\'']);
            if label != "token" && value_len(value) >= 8 {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_folds_space_and_typography() {
        assert_eq!(normalize("  a \u{00A0}\n b\u{200B}c  "), "a bc");
        assert_eq!(
            normalize("It\u{2019}s \u{201C}x\u{201D} \u{2013} y\u{2026}"),
            "It's \"x\" - y..."
        );
    }

    #[test]
    fn the_text_must_be_long_enough_and_the_owners_own_words() {
        let owner = "Please remember that I write to colleagues  in Slovak.";
        assert_eq!(
            owner_span("i WRITE to colleagues in slovak", owner).as_deref(),
            Ok("I write to colleagues in Slovak")
        );
        assert_eq!(owner_span("in Slovak", owner), Err(QuoteCheck::TooShort));
        assert_eq!(
            owner_span("I write to colleagues in German", owner),
            Err(QuoteCheck::NotFound)
        );
        assert_eq!(
            owner_span("I write to colleagues in Slovak and more", owner),
            Err(QuoteCheck::NotFound)
        );
    }

    #[test]
    fn a_remember_request_is_found_in_code_ignoring_case_and_diacritics() {
        for yes in [
            "Please REMEMBER that I like tea",
            "Don\u{2019}t forget my address",
            "zapamätaj si, že pijem čaj",
            "ZAPAMATAJ si to",
            "Pamätaj si moje meno",
            "nezabudni na to",
            "Poznač si: stretnutie o piatej",
            "keep in mind I prefer tea",
            "note that I prefer tea",
        ] {
            assert!(has_remember_request(yes), "{yes}");
        }
        for no in [
            "I prefer tea in the morning",
            "this is memorable",
            "zapamatajte",
            "",
        ] {
            assert!(!has_remember_request(no), "{no}");
        }
    }

    #[test]
    fn size_and_secrets_are_refused() {
        assert_eq!(screen_text("  "), Err(GateRefusal::Empty));
        assert_eq!(screen_text(&"a".repeat(601)), Err(GateRefusal::TooLong));
        for secret in [
            "my key is sk-abcdefghijklmnop",
            "token ghp_abcdefghijklmnopqrstuvwx",
            "password: hunter2hunter2",
            "the password is hunter2hunter2",
            "Authorization: Bearer abcdefghijklmnop",
            "AKIAABCDEFGHIJKLMNOP is the id",
            "-----BEGIN RSA PRIVATE KEY-----",
            "hf_abcdefghijklmnopqrstuvwxyz",
            "npm_abcdefghijklmnopqrstuvwx",
            "shpat_abcdef0123456789abcdef",
            "shpss_abcdef0123456789abcdef",
            "dop_v1_abcdef0123456789abcdef",
            "xoxs-1234567890-abcdefghij",
            "xoxe-1234567890-abcdefghij",
            "glpat-abcdefghij0123456789",
            "sk-ant-api03-abcdefghijklmn",
            "ghs_abcdefghijklmnopqrstuvwx",
            "github_pat_11ABCDEFG0abcdefghij",
            "api key: abc!def@ghi#jkl",
            "my access token is ab12!cd34@ef56",
            "secret key = Zq9$Zq9$Zq9$",
            "private key: abc/def+ghi==",
            "heslo: Tajne!Heslo#1",
            "moje heslo je Tajne!Heslo#1",
            "prístupový kľúč: abcd-efgh-ijkl",
            "password: p@ss!w0rd#long",
            "sk-\u{200B}abcdefghijklmnop",
        ] {
            assert_eq!(screen_text(secret), Err(GateRefusal::Secret), "{secret}");
        }
        for fine in [
            "Works in Rust for backend services.",
            "The token budget is 1500 per request.",
            "Prefers short answers; the secret sauce is garlic.",
        ] {
            assert_eq!(screen_text(fine), Ok(()), "{fine}");
        }
    }
}
