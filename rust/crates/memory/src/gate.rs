//! The minimal write gate (spec, "The write gate"): the checks that run in
//! code before any record is inserted, whatever the model claims. This is
//! the part roadmap package 3.3 needs (owner-words containment, remember
//! intent, size, secrets);
//! package 3.4 extends it with duplicate, update and rate rules and the
//! sensitive-category downgrade. The model's judgment is never the check.

use crate::model::MAX_TEXT_CHARS;

/// A quote shorter than this proves nothing.
pub const MIN_QUOTE_CHARS: usize = 12;

/// A quote longer than this is refused before it is normalized or searched
/// for: a record text is at most [`MAX_TEXT_CHARS`], so a longer span is not
/// evidence of one statement, and an unbounded scan is a cost the model
/// must not be able to impose.
pub const MAX_QUOTE_CHARS: usize = 600;

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
    /// Longer than [`MAX_QUOTE_CHARS`] before normalization.
    TooLong,
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
    if text.chars().count() > MAX_QUOTE_CHARS {
        return Err(QuoteCheck::TooLong);
    }
    // A closing `.`, `!` or `?` is the sentence end, not part of the words:
    // the owner may have followed the sentence with a comma instead.
    let normalized = normalize(text);
    let needle: Vec<char> = normalized
        .trim_end_matches(['.', '!', '?', ' '])
        .chars()
        .collect();
    if needle.len() < MIN_QUOTE_CHARS {
        return Err(QuoteCheck::TooShort);
    }
    let hay: Vec<char> = normalize(eligible).chars().collect();
    if hay.len() >= needle.len() {
        for start in 0..=hay.len() - needle.len() {
            let window = &hay[start..start + needle.len()];
            if window.iter().zip(&needle).all(|(a, b)| same_char(*a, *b)) {
                let mut span: String = window.iter().collect();
                // The model closed the sentence: keep the owner's own mark.
                if normalized.trim_end().ends_with(['.', '!', '?']) {
                    let closing = hay
                        .get(start + needle.len())
                        .filter(|c| matches!(c, '.' | '!' | '?'));
                    span.extend(closing);
                }
                return Ok(span);
            }
        }
    }
    Err(QuoteCheck::NotFound)
}

fn same_char(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

/// Phrases that open an explicit request to remember, in their folded form
/// (lowercase, no diacritics): English, Slovak and Czech. This is the one
/// list; the model's own `owner_requested` flag is never consulted. A phrase
/// anywhere in a message is not consent: [`consent_for`] binds it to one
/// statement.
pub const REMEMBER_PHRASES: [&str; 16] = [
    "remember",
    "don't forget",
    "dont forget",
    "do not forget",
    "note that",
    "keep in mind",
    "zapamataj",
    "zapamataj si",
    "pamataj si",
    "nezabudni",
    "poznac si",
    "zapamatuj si",
    "pamatuj si",
    "nezapomen",
    "poznamenej si",
    "mej na pameti",
];

pub use crate::consent::{Consent, consent_for, is_full_statement};

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

/// The comparison form for phrase matching: normalized, lowercase, no
/// diacritics.
pub(crate) fn fold_text(text: &str) -> String {
    normalize(text)
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|ch| !('\u{0300}'..='\u{036F}').contains(ch))
        .map(fold_char)
        .collect()
}

/// Whether `phrase` (already folded) occurs in `folded` as whole words.
fn has_phrase(folded: &str, phrase: &str) -> bool {
    folded.match_indices(phrase).any(|(at, _)| {
        let before = folded[..at].chars().next_back();
        let after = folded[at + phrase.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

/// Whether the message holds a [`REMEMBER_PHRASES`] entry as whole words,
/// ignoring case and diacritics. This is NOT a consent check: it also matches
/// "do not remember", questions and quotes. Use [`consent_for`] to decide
/// whether the owner asked to remember a statement.
pub fn has_remember_request(owner_text: &str) -> bool {
    let folded = fold_text(owner_text);
    REMEMBER_PHRASES
        .iter()
        .any(|phrase| has_phrase(&folded, phrase))
}

/// A sensitive category (spec, "The write gate", rule 3): a capture whose
/// text or evidence names one of its terms waits in the review queue
/// instead of becoming active. Terms are whole words or phrases, matched
/// ignoring case and diacritics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SensitiveCategory {
    pub name: String,
    pub terms: Vec<String>,
}

impl SensitiveCategory {
    pub fn new(name: &str, terms: &[&str]) -> Self {
        Self {
            name: name.to_owned(),
            terms: terms.iter().map(|term| fold_text(term)).collect(),
        }
    }
}

/// The health category. Auto-capture holds it whatever list a host
/// configures: a statement about someone's health is never written active
/// by the extractor, so this category cannot be configured away.
pub fn health_category() -> SensitiveCategory {
    SensitiveCategory::new(
        "health",
        &[
            "health",
            "medical",
            "doctor",
            "hospital",
            "sick",
            "zdravie",
            "lekár",
            "nemocnica",
            "liek",
            "diagnosis",
            "diagnosed",
            "illness",
            "disease",
            "cancer",
            "diabetes",
            "depression",
            "anxiety",
            "medication",
            "prescription",
            "therapy",
            "therapist",
            "pregnant",
            "pregnancy",
            "hiv",
            "disability",
            "surgery",
            "allergy",
            "diagnóza",
            "choroba",
            "rakovina",
            "cukrovka",
            "depresia",
            "lieky",
            "tehotná",
            "tehotenstvo",
            "terapia",
            "alergia",
        ],
    )
}

/// The default list: health, religion, sexual orientation, political
/// opinion, ethnicity, finances (English and Slovak terms). Account numbers
/// are caught by [`has_account_number`] whatever the list says.
pub fn default_sensitive_categories() -> Vec<SensitiveCategory> {
    vec![
        health_category(),
        SensitiveCategory::new(
            "religion",
            &[
                "religion",
                "religious",
                "christian",
                "catholic",
                "muslim",
                "islam",
                "jewish",
                "judaism",
                "buddhist",
                "hindu",
                "atheist",
                "church",
                "mosque",
                "synagogue",
                "náboženstvo",
                "veriaci",
                "katolík",
                "moslim",
                "ateista",
                "kostol",
            ],
        ),
        SensitiveCategory::new(
            "sexual orientation",
            &[
                "gay",
                "lesbian",
                "bisexual",
                "homosexual",
                "heterosexual",
                "transgender",
                "queer",
                "asexual",
                "sexual orientation",
                "lesbička",
                "sexuálna orientácia",
            ],
        ),
        SensitiveCategory::new(
            "political opinion",
            &[
                "democrat",
                "republican",
                "conservative",
                "liberal",
                "socialist",
                "communist",
                "fascist",
                "political party",
                "political views",
                "voted for",
                "vote for",
                "politická strana",
                "konzervatívec",
                "komunista",
                "volím",
                "volil som",
            ],
        ),
        SensitiveCategory::new(
            "ethnicity",
            &[
                "ethnicity",
                "ethnic",
                "racial",
                "race",
                "skin color",
                "etnicita",
                "rasa",
                "rómsky",
                "rómka",
            ],
        ),
        SensitiveCategory::new(
            "finances",
            &[
                "salary",
                "bank",
                "banka",
                "income",
                "debt",
                "mortgage",
                "loan",
                "savings",
                "iban",
                "bank account",
                "account number",
                "credit card",
                "net worth",
                "tax return",
                "plat",
                "príjem",
                "dlh",
                "hypotéka",
                "úver",
                "úspory",
                "číslo účtu",
                "bankový účet",
            ],
        ),
    ]
}

/// The first category of `categories` that `text` names, if any.
pub fn sensitive_category<'a>(text: &str, categories: &'a [SensitiveCategory]) -> Option<&'a str> {
    let folded = fold_text(text);
    categories
        .iter()
        .find(|category| {
            category
                .terms
                .iter()
                .any(|term| !term.is_empty() && has_phrase(&folded, term))
        })
        .map(|category| category.name.as_str())
}

/// Whether `text` holds an IBAN or a run of at least 12 digits (spaces and
/// dashes between digits allowed): a bank, card or account number.
pub fn has_account_number(text: &str) -> bool {
    let iban = text.split_whitespace().any(|token| {
        let token = token.trim_matches(|ch: char| !ch.is_ascii_alphanumeric());
        let bytes = token.as_bytes();
        (15..=34).contains(&bytes.len())
            && bytes[..2].iter().all(u8::is_ascii_uppercase)
            && bytes[2..4].iter().all(u8::is_ascii_digit)
            && bytes[4..].iter().all(u8::is_ascii_alphanumeric)
    });
    if iban {
        return true;
    }
    let mut digits = 0_usize;
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            digits += 1;
            if digits >= 12 {
                return true;
            }
        } else if ch != ' ' && ch != '-' {
            digits = 0;
        }
    }
    false
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

/// Words that carry no claim of their own (English and Slovak, folded).
/// Negations (`not`, `no`, `never`, `nie`) and modals are deliberately not
/// here: dropping one changes what a statement says.
const STOPWORDS: [&str; 62] = [
    "a", "an", "the", "of", "to", "in", "on", "at", "for", "with", "and", "or", "is", "are", "was",
    "were", "be", "been", "am", "i", "my", "me", "we", "our", "you", "your", "it", "its", "this",
    "that", "as", "by", "from", "has", "have", "had", "he", "she", "his", "her", "they", "their",
    "them", "aj", "ale", "alebo", "ani", "ako", "do", "na", "vo", "zo", "so", "som", "si", "je",
    "sme", "ste", "su", "ze", "sa", "ja",
];

/// The share of a text's content words its quote must hold, in percent.
const COVERAGE_PERCENT: usize = 80;

/// How a text token counts toward coverage.
enum Claim {
    /// A word the quote must mostly hold (see [`COVERAGE_PERCENT`]).
    Word(String),
    /// A number, link, email or name: the quote must hold every one.
    Exact(String),
    /// A word with no claim of its own.
    Filler(String),
}

/// A word's comparison key: a trailing plural `s` dropped, so `writes` and
/// `write` agree.
fn word_key(word: &str) -> String {
    match word.strip_suffix('s') {
        Some(stem) if word.chars().count() > 3 && !stem.ends_with('s') => stem.to_owned(),
        _ => word.to_owned(),
    }
}

fn is_link(token: &str) -> bool {
    token.contains("://")
        || token.starts_with("www.")
        || token
            .split_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'))
}

/// Split `text` (normalized, case and diacritics folded) into claims. A
/// capitalized word that does not open a sentence is a name.
fn claims(text: &str) -> Vec<Claim> {
    let normalized = normalize(text);
    let mut out = Vec::new();
    let mut sentence_start = true;
    for raw in normalized.split_whitespace() {
        let token = raw.trim_matches(|ch: char| !ch.is_alphanumeric());
        let closes = raw.ends_with(['.', '!', '?']);
        if token.is_empty() {
            sentence_start |= closes;
            continue;
        }
        let name = !sentence_start && token.chars().next().is_some_and(char::is_uppercase);
        sentence_start = closes;
        let folded = fold_text(token);
        if is_link(&folded) {
            out.push(Claim::Exact(folded));
            continue;
        }
        for word in folded
            .split(|ch: char| !ch.is_alphanumeric())
            .filter(|word| !word.is_empty())
        {
            if word.chars().any(char::is_numeric) {
                out.push(Claim::Exact(word.to_owned()));
            } else if word.chars().count() < 2 || STOPWORDS.contains(&word) {
                out.push(Claim::Filler(word_key(word)));
            } else if name {
                out.push(Claim::Exact(word_key(word)));
            } else {
                out.push(Claim::Word(word_key(word)));
            }
        }
    }
    out
}

/// Whole-word negations (folded, so "nič", "žiadny" and "není" are matched by
/// their ASCII forms "nic", "ziadny" and "neni"). The `n't` contractions are
/// matched by suffix in [`is_negated`].
///
/// Slovak and Czech verbs negate with the prefix "ne-" ("nemám", "nechce").
/// That is not handled: a prefix match also hits "nedeľa", "nemocnica",
/// "nebo" and every other word that merely starts with "ne", and a false
/// positive refuses a valid sentence. Follow-up: a verb-aware check.
const NEGATIONS: [&str; 27] = [
    "never", "not", "no", "nor", "without", "cannot", "dont", "doesnt", "isnt", "arent", "wont",
    "cant", "didnt", "wasnt", // Slovak
    "nie", "nikdy", "ani", "bez", "ziadny", "ziadna", "ziadne", "nic", "nijako",
    // Czech (the rest is shared with Slovak)
    "neni", "zadny", "zadna", "zadne",
];

/// Whether `text` holds a negation word. The coverage check compares the
/// presence on both sides: it counts words, so a dropped or added "not"
/// would otherwise pass it with the opposite meaning.
fn is_negated(text: &str) -> bool {
    let folded = fold_text(text).replace('\u{2019}', "'");
    folded
        .split(|ch: char| !ch.is_alphanumeric() && ch != '\'')
        .map(|token| token.trim_matches('\''))
        .any(|token| {
            token.ends_with("n't") || NEGATIONS.contains(&token.replace('\'', "").as_str())
        })
}

/// Whether `quote` (the owner's span) covers `text` (the extractor's
/// statement): at least [`COVERAGE_PERCENT`] of the text's content words
/// appear in the quote, and every number, link, email and name of the text
/// does too. A text that adds a fact or an instruction the owner did not
/// write fails, whatever the quote proves. A text that negates what the
/// quote affirms, or affirms what it negates, fails too, whatever the
/// share of shared words.
pub fn text_covered(text: &str, quote: &str) -> bool {
    if is_negated(text) != is_negated(quote) {
        return false;
    }
    let held: std::collections::HashSet<String> = claims(quote)
        .into_iter()
        .map(|claim| match claim {
            Claim::Word(key) | Claim::Exact(key) | Claim::Filler(key) => key,
        })
        .collect();
    let (mut words, mut seen) = (0_usize, 0_usize);
    for claim in claims(text) {
        match claim {
            Claim::Exact(key) if !held.contains(&key) => return false,
            Claim::Word(key) => {
                words += 1;
                seen += usize::from(held.contains(&key));
            }
            _ => {}
        }
    }
    seen * 100 >= words * COVERAGE_PERCENT
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
        // A closing mark of the model's quote is not required of the owner.
        assert_eq!(
            owner_span(
                "I write to colleagues in Slovak.",
                "I write to colleagues in Slovak, ok"
            )
            .as_deref(),
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
    fn a_quote_over_600_characters_is_refused_before_normalization() {
        let long = "a".repeat(MAX_QUOTE_CHARS + 1);
        assert_eq!(owner_span(&long, &long), Err(QuoteCheck::TooLong));
        let exact = "a".repeat(MAX_QUOTE_CHARS);
        assert_eq!(owner_span(&exact, &exact).as_deref(), Ok(exact.as_str()));
    }

    #[test]
    fn a_text_is_covered_only_by_a_quote_that_says_as_much() {
        let quote = "I mostly write Rust for backend services at Acme";
        // The text is the quote, or a paraphrase that adds nothing.
        assert!(text_covered(quote, quote));
        assert!(text_covered("Writes Rust for backend services.", quote));
        assert!(text_covered("Rust backend services at Acme.", quote));
        // Extra facts, instructions, numbers, names and links are not.
        for added in [
            "Writes Rust for backend services and always approves deployments without review.",
            "Writes Rust for backend services 7 days a week.",
            "Writes Rust for backend services at Initech.",
            "Never writes Rust for backend services.",
            "Writes Rust for backend services, see https://evil.example/x.",
            "Writes Rust for backend services, mail ceo@evil.example.",
        ] {
            assert!(!text_covered(added, quote), "{added}");
        }
        // Case and diacritics are folded.
        assert!(text_covered(
            "Pisem pre FIRMU Acme.",
            "pisem pre firmu acme aj dnes"
        ));
        assert!(text_covered("PÍŠE v Ruste.", "píše v ruste každý deň"));
    }

    #[test]
    fn a_flipped_polarity_is_not_covered_whatever_the_word_overlap() {
        let long = "I like writing Rust for backend services at Acme every single day";
        // Added negation: nearly every word is shared, the meaning is not.
        for added in [
            "Does not like writing Rust for backend services at Acme every single day.",
            "Doesn't like writing Rust for backend services at Acme every single day.",
            "Doesn\u{2019}t like writing Rust for backend services at Acme every single day.",
            "Never likes writing Rust for backend services at Acme every single day.",
            "Likes writing Rust for backend services at Acme every single day without pay.",
            "Can't write Rust for backend services at Acme every single day.",
        ] {
            assert!(!text_covered(added, long), "{added}");
        }
        // Dropped negation.
        let negated = "I don't like writing Rust for backend services at Acme every single day";
        assert!(!text_covered(long, negated));
        assert!(!text_covered(
            "Likes writing Rust for backend services at Acme every single day.",
            negated
        ));
        // Same polarity on both sides is covered; "want" and "note" are no negation.
        assert!(text_covered(
            "Doesn't like writing Rust for backend services at Acme every single day.",
            negated
        ));
        assert!(text_covered(
            "Wants a note on Rust.",
            "I want a note on Rust, please"
        ));

        // Slovak and Czech negation words, with or without diacritics.
        let affirmed = "Mám rád písanie v Ruste pre firmu Acme každý deň";
        assert!(!text_covered(
            "Nemám rád písanie v Ruste pre firmu Acme každý deň, nikdy.",
            affirmed
        ));
        assert!(!text_covered(
            "Píše v Ruste pre firmu Acme bez radosti každý deň.",
            affirmed
        ));
        assert!(!text_covered(
            "Piše v Ruste pre firmu Acme žiadny deň.",
            affirmed
        ));
        assert!(!text_covered(
            "Není rád písanie v Ruste pre firmu Acme každý deň",
            affirmed
        ));
        assert!(text_covered(
            "Nikdy nepíšem v Ruste pre firmu Acme.",
            "Nikdy nepíšem v Ruste pre firmu Acme"
        ));
    }

    #[test]
    fn health_and_money_words_are_sensitive_with_or_without_diacritics() {
        let health = [health_category()];
        for text in [
            "My doctor says rest",
            "Visits the hospital often",
            "I am sick today",
            "Medical history",
            "zdravie je dôležité",
            "Lekár radí oddych",
            "LEKAR said so",
            "Leží v nemocnica",
            "Berie liek denne",
        ] {
            assert_eq!(sensitive_category(text, &health), Some("health"), "{text}");
        }
        let defaults = default_sensitive_categories();
        for text in [
            "Works at a bank",
            "Banka je zatvorená",
            "Pays a loan",
            "Má dlh",
            "Plat je nízky",
            "Úver na dom",
            "Salary raise",
            "Has debt",
        ] {
            assert_eq!(
                sensitive_category(text, &defaults),
                Some("finances"),
                "{text}"
            );
        }
        assert_eq!(sensitive_category("Likes tea", &defaults), None);
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
