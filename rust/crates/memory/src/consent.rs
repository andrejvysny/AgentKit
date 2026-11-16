//! W1 consent binding and W2 meaning (review R01, R14). A remember phrase
//! somewhere in the owner's message proves nothing about one statement: the
//! owner may have said "do not remember that ...", asked a question, quoted
//! somebody, or asked to remember something else. [`consent_for`] decides, in
//! code, whether the owner asked to remember exactly this candidate, asked
//! not to, or did neither. [`is_full_statement`] decides whether a candidate
//! is one complete sentence the owner wrote.
//!
//! Deliberately simple: sentences end at `.`, `!`, `?` (followed by space or
//! the end) or a newline, abbreviations are not handled, and a doubtful case
//! is [`Consent::Absent`], which only ever means "wait for approval".

use crate::gate::{REMEMBER_PHRASES, fold_text, normalize};

/// What the owner's message says about remembering one candidate text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consent {
    /// The owner asked, in the imperative, to remember exactly this.
    Granted,
    /// The owner asked not to remember it (or to forget it).
    Refused,
    /// Neither: no request, or a request that is not bound to this text.
    Absent,
}

/// Words that may stand before a request phrase in its clause. The English
/// pronoun `i` is deliberately absent: "I remember when ..." is not a request.
const LEAD: [&str; 18] = [
    "please", "pls", "and", "also", "so", "prosim", "a", "tak", "do", "ever", "kindly", "just",
    "not", "never", "dont", "don't", "nie", "nikdy",
];
const NEGATIONS: [&str; 6] = ["not", "never", "dont", "don't", "nie", "nikdy"];
/// `nezabudni` and `nezapomen` are requests (REMEMBER_PHRASES), not here.
const REFUSAL_VERBS: [&str; 7] = [
    "forget",
    "zabudni",
    "zapomen",
    "nezapamataj",
    "nepamataj",
    "nezapamatuj",
    "nepamatuj",
];
/// What a request may name instead of a statement ("remember this").
const BARE: [&str; 7] = ["this", "it", "to", "toto", "please", "pls", "prosim"];

struct Sentence {
    /// The sentence in comparison form: normalized, lowercase, without the
    /// closing `.`, `!` or `?`.
    lc: String,
    question: bool,
    /// Whether a quote opened in an earlier sentence is still open.
    quoted_at_start: bool,
}

/// The comparison form of a statement: normalized, lowercase, trailing
/// `.`, `!` and `?` ignored.
fn key(text: &str) -> String {
    normalize(text)
        .to_lowercase()
        .trim()
        .trim_end_matches(['.', '!', '?', ' '])
        .to_owned()
}

/// Whether an odd number of quote marks is open at the end of `text`. An
/// apostrophe inside a word ("don't") is not a quote mark.
fn quote_parity(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let mut open = false;
    for (at, &ch) in chars.iter().enumerate() {
        let quote = match ch {
            '"' => true,
            '\'' => {
                let inner = at > 0
                    && chars[at - 1].is_alphanumeric()
                    && chars.get(at + 1).is_some_and(|next| next.is_alphanumeric());
                !inner
            }
            _ => false,
        };
        open ^= quote;
    }
    open
}

fn split_sentences(text: &str) -> Vec<Sentence> {
    let mut pieces: Vec<String> = Vec::new();
    for line in text.split(['\n', '\r']) {
        let chars: Vec<char> = line.chars().collect();
        let mut current = String::new();
        let mut at = 0;
        while at < chars.len() {
            current.push(chars[at]);
            if matches!(chars[at], '.' | '!' | '?') {
                let mut end = at + 1;
                while end < chars.len()
                    && matches!(chars[end], '"' | '\'' | '\u{201D}' | '\u{2019}')
                {
                    end += 1;
                }
                if end == chars.len() || chars[end].is_whitespace() {
                    current.extend(&chars[at + 1..end]);
                    pieces.push(std::mem::take(&mut current));
                    at = end;
                    continue;
                }
            }
            at += 1;
        }
        pieces.push(current);
    }
    let mut open = false;
    let mut out = Vec::new();
    for piece in pieces {
        let lc = key(&piece);
        if lc.is_empty() {
            continue;
        }
        let closed = piece.trim_end_matches(['"', '\'', '\u{201D}', '\u{2019}', ' ']);
        out.push(Sentence {
            question: closed.ends_with('?'),
            quoted_at_start: open,
            lc: lc.clone(),
        });
        open ^= quote_parity(&lc);
    }
    out
}

struct Tok {
    /// Folded word, punctuation trimmed.
    word: String,
    /// Byte offsets in the sentence: the token, and its word.
    start: usize,
    core_start: usize,
    core_end: usize,
    /// The token opens a clause (sentence start, or after `,` `;` `:` `-`).
    clause_start: bool,
}

fn is_edge(ch: char) -> bool {
    !ch.is_alphanumeric()
}

fn tokens(lc: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let (mut offset, mut opens) = (0_usize, true);
    for raw in lc.split(' ') {
        let start = offset;
        offset += raw.len() + 1;
        if raw.is_empty() {
            continue;
        }
        let core = raw.trim_matches(is_edge);
        if core.is_empty() {
            if raw.chars().all(|ch| matches!(ch, '-' | ',' | ';' | ':')) {
                opens = true;
            }
            continue;
        }
        let lead = raw.len() - raw.trim_start_matches(is_edge).len();
        out.push(Tok {
            word: fold_text(core),
            start,
            core_start: start + lead,
            core_end: start + lead + core.len(),
            clause_start: opens,
        });
        opens = raw.ends_with([',', ';', ':']);
    }
    out
}

/// One request (or refusal) clause of a sentence.
struct Req {
    refusal: bool,
    /// Byte offset where the clause begins, and where its phrase ends.
    clause_start: usize,
    after: usize,
}

fn phrase_len_at(clause: &[Tok], at: usize) -> Option<usize> {
    let mut phrases: Vec<Vec<&str>> = REMEMBER_PHRASES
        .iter()
        .map(|phrase| phrase.split(' ').collect())
        .collect();
    phrases.sort_by_key(|words| std::cmp::Reverse(words.len()));
    phrases.into_iter().find_map(|words| {
        let window = clause.get(at..at + words.len())?;
        window
            .iter()
            .zip(&words)
            .all(|(tok, word)| tok.word == *word)
            .then_some(words.len())
    })
}

fn quote_open(sentence: &Sentence, at: usize) -> bool {
    sentence.quoted_at_start ^ quote_parity(&sentence.lc[..at])
}

/// The request in one clause: a remember phrase or a refusal verb that is
/// imperative (only filler and negation words before it) and not quoted.
fn clause_request(sentence: &Sentence, clause: &[Tok]) -> Option<Req> {
    for at in 0..clause.len() {
        let phrase = phrase_len_at(clause, at);
        let verb = REFUSAL_VERBS.contains(&clause[at].word.as_str());
        if phrase.is_none() && !verb {
            continue;
        }
        if quote_open(sentence, clause[at].core_start) {
            return None;
        }
        let lead = &clause[..at];
        if !lead.iter().all(|tok| LEAD.contains(&tok.word.as_str())) {
            return None;
        }
        let negated = lead
            .iter()
            .any(|tok| NEGATIONS.contains(&tok.word.as_str()));
        return Some(Req {
            refusal: verb && phrase.is_none() || negated,
            clause_start: clause[0].start,
            after: clause[at + phrase.unwrap_or(1) - 1].core_end,
        });
    }
    None
}

fn requests(sentence: &Sentence) -> Vec<Req> {
    if sentence.question {
        return Vec::new();
    }
    let toks = tokens(&sentence.lc);
    let mut out = Vec::new();
    let mut from = 0;
    while from < toks.len() {
        let mut to = from + 1;
        while to < toks.len() && !toks[to].clause_start {
            to += 1;
        }
        out.extend(clause_request(sentence, &toks[from..to]));
        from = to;
    }
    out
}

const SEPARATORS: [char; 5] = [' ', ',', ':', ';', '-'];

/// What a request governs.
enum Governed {
    /// This text: what follows the phrase, or what precedes a trailing one.
    Text(String),
    /// The whole next sentence ("Remember this. I live in Bratislava.").
    NextSentence,
}

fn governed(sentence: &Sentence, reqs: &[Req], at: usize) -> Governed {
    let req = &reqs[at];
    let end = reqs
        .get(at + 1)
        .map_or(sentence.lc.len(), |next| next.clause_start)
        .max(req.after);
    let mut rest = sentence.lc[req.after..end].trim_start_matches(SEPARATORS);
    let word = rest.split(' ').next().unwrap_or_default();
    let bare_label = word.ends_with(':')
        && matches!(
            fold_text(word.trim_matches(is_edge)).as_str(),
            "this" | "it" | "toto"
        );
    let connector = matches!(
        fold_text(word.trim_matches(is_edge)).as_str(),
        "that" | "ze"
    );
    if bare_label || connector {
        rest = rest[word.len()..].trim_start_matches(SEPARATORS);
    }
    let rest = rest.trim_end_matches(SEPARATORS);
    let bare = rest
        .split(' ')
        .filter(|w| !w.is_empty())
        .all(|w| BARE.contains(&fold_text(w.trim_matches(is_edge)).as_str()))
        && !bare_label;
    if !bare {
        return Governed::Text(rest.to_owned());
    }
    let before = sentence.lc[..req.clause_start].trim_end_matches(SEPARATORS);
    if before.is_empty() {
        Governed::NextSentence
    } else {
        Governed::Text(before.to_owned())
    }
}

/// Whether `needle` occurs in `hay` as whole words.
fn contains_words(hay: &str, needle: &str) -> bool {
    hay.match_indices(needle).any(|(at, _)| {
        let before = hay[..at].chars().next_back();
        let after = hay[at + needle.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

/// Whether `owner_text` asks to remember exactly `candidate`, asks not to, or
/// does neither. A candidate is [`Consent::Granted`] only when it equals what
/// an imperative request governs: the rest of the request sentence, the part
/// before a trailing "..., remember that", or the whole next sentence after a
/// bare "remember this". A part of that statement is not granted (dropping a
/// word could drop a negation). Questions and quoted requests never grant,
/// and any sentence that holds the candidate and refuses is
/// [`Consent::Refused`], whatever else the message says.
pub fn consent_for(candidate: &str, owner_text: &str) -> Consent {
    let wanted = key(candidate);
    if wanted.is_empty() {
        return Consent::Absent;
    }
    let sentences = split_sentences(owner_text);
    let reqs: Vec<Vec<Req>> = sentences.iter().map(requests).collect();
    let next_is_wanted = |at: usize| {
        sentences
            .get(at + 1)
            .is_some_and(|next| !next.question && next.lc == wanted)
    };
    let mut granted = false;
    for (idx, sentence) in sentences.iter().enumerate() {
        let list = &reqs[idx];
        if list.iter().any(|req| req.refusal) && contains_words(&sentence.lc, &wanted) {
            return Consent::Refused;
        }
        for (at, req) in list.iter().enumerate() {
            let hit = match governed(sentence, list, at) {
                Governed::Text(text) => key(&text) == wanted,
                Governed::NextSentence => next_is_wanted(idx),
            };
            match (hit, req.refusal) {
                (true, true) => return Consent::Refused,
                (true, false) => granted = true,
                _ => {}
            }
        }
    }
    if granted {
        Consent::Granted
    } else {
        Consent::Absent
    }
}

/// W2 meaning: `text` is one complete sentence of `owner_text` (case and the
/// closing `.`, `!`, `?` ignored), that sentence is not a question, and it
/// does not ask to forget or not to remember. A paraphrase, a clause cut out
/// of a longer sentence or a sentence with a word dropped is not.
pub fn is_full_statement(text: &str, owner_text: &str) -> bool {
    let wanted = key(text);
    !wanted.is_empty()
        && split_sentences(owner_text).iter().any(|sentence| {
            !sentence.question
                && sentence.lc == wanted
                && !requests(sentence).iter().any(|req| req.refusal)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn consent(candidate: &str, owner: &str) -> Consent {
        consent_for(candidate, owner)
    }

    #[test]
    fn refusals_in_three_languages_are_refused() {
        for (candidate, owner) in [
            (
                "I live in Bratislava.",
                "Do not remember that I live in Bratislava.",
            ),
            (
                "I live in Bratislava",
                "Don't remember that I live in Bratislava.",
            ),
            (
                "I live in Bratislava",
                "Don\u{2019}t remember that I live in Bratislava.",
            ),
            (
                "I live in Bratislava",
                "Please never remember that I live in Bratislava.",
            ),
            ("I live in Bratislava", "Forget that I live in Bratislava."),
            ("Bývam v Bratislave", "Nepamätaj si, že bývam v Bratislave."),
            (
                "Bývam v Bratislave",
                "Nezapamätaj si, že bývam v Bratislave.",
            ),
            ("Bydlím v Brně", "Nezapamatuj si, že bydlím v Brně."),
            ("Bydlím v Brně", "Zapomeň, že bydlím v Brně."),
            (
                "I live in Bratislava",
                "I live in Bratislava, don't remember that.",
            ),
            (
                "I live in Bratislava",
                "Do not remember this. I live in Bratislava.",
            ),
            (
                "I live in Bratislava",
                "Remember that I live in Bratislava. Do not remember that I live in Bratislava.",
            ),
        ] {
            assert_eq!(consent(candidate, owner), Consent::Refused, "{owner}");
        }
    }

    #[test]
    fn imperative_requests_are_granted() {
        for (candidate, owner) in [
            (
                "I live in Bratislava.",
                "Remember that I live in Bratislava.",
            ),
            ("I am vegan", "Please remember: I am vegan."),
            ("Som vegán", "Zapamätaj si, že som vegán."),
            ("Jsem vegan", "Zapamatuj si, že jsem vegan."),
            (
                "I live in Bratislava",
                "I live in Bratislava, remember that.",
            ),
            (
                "I live in Bratislava",
                "Remember this. I live in Bratislava.",
            ),
            ("Mám psa", "Nezabudni, že mám psa."),
            ("I live in Bratislava", "Note that I live in Bratislava."),
            ("Mám psa", "Mám psa - nezabudni."),
            ("I like tea", "Hi! Please, remember that I like tea."),
            ("I like tea", "Remember this: I like tea."),
            ("I live in Bratislava", "remember\nI live in Bratislava"),
        ] {
            assert_eq!(consent(candidate, owner), Consent::Granted, "{owner}");
        }
    }

    #[test]
    fn questions_quotes_other_clauses_and_partials_never_grant() {
        for (candidate, owner) in [
            (
                "I live in Bratislava",
                "Do you remember that I live in Bratislava?",
            ),
            (
                "I live in Bratislava",
                "He told me 'remember that I live in Bratislava'",
            ),
            (
                "I live in Bratislava",
                "He told me \"remember that I live in Bratislava\".",
            ),
            (
                "I live in Bratislava",
                "Remember to buy milk. I live in Bratislava.",
            ),
            (
                "live in Bratislava",
                "Remember that I don't live in Bratislava.",
            ),
            (
                "I live in Bratislava",
                "I remember that I live in Bratislava.",
            ),
            ("I live in Bratislava", "I live in Bratislava."),
        ] {
            assert_eq!(consent(candidate, owner), Consent::Absent, "{owner}");
        }
    }

    #[test]
    fn a_phrase_inside_a_word_or_the_request_of_something_else_is_no_consent() {
        assert_eq!(
            consent(
                "I live in Bratislava",
                "Remember Alice. I live in Bratislava."
            ),
            Consent::Absent
        );
        assert_eq!(
            consent(
                "I live in Bratislava",
                "This is memorable: I live in Bratislava."
            ),
            Consent::Absent
        );
    }

    #[test]
    fn full_statement_needs_one_whole_sentence() {
        let owner = "My brother prefers coffee. I prefer tea in the morning. Do you like tea?";
        assert!(is_full_statement("I prefer tea in the morning.", owner));
        assert!(is_full_statement("i prefer TEA in the morning", owner));
        assert!(!is_full_statement("I prefer coffee.", owner));
        assert!(!is_full_statement("Do you like tea?", owner));
        assert!(!is_full_statement("prefer tea in the morning", owner));
        assert!(!is_full_statement(
            "I prefer coffee in the morning.",
            "I do not prefer coffee in the morning."
        ));
        assert!(!is_full_statement(
            "I would live in Vienna",
            "If I move, I would live in Vienna."
        ));
        assert!(!is_full_statement(
            "Bývam v Bratislave.",
            "Nepamätaj si, že bývam v Bratislave."
        ));
        assert!(!is_full_statement(
            "I live in Bratislava.",
            "Forget I live in Bratislava."
        ));
        assert!(is_full_statement(
            "Nemám rád kávu.",
            "Nemám rád kávu. Mám rád čaj."
        ));
    }
}
