//! Query-side term expansion for lexical search, shared by every
//! `RetrievalIndex` adapter so the SQLite index and its twin agree.
//!
//! Slovak and Czech inflect nouns ("veterinárovi", "veterinára"), which
//! defeats plain prefix matching of the query word. Each long query word
//! therefore also yields a prefix term with a common case or number ending
//! stripped. Function words are dropped. No index change, no synonyms.

use crate::LexicalQuery;

/// Words shorter than this get no stem.
const STEM_MIN_WORD_CHARS: usize = 5;
/// A stem is never shorter than this.
const STEM_MIN_CHARS: usize = 4;
const MAX_QUERY_CHARS: usize = 1_000;
const MAX_QUERY_WORDS: usize = 32;

/// Longest first, so "-ovi" wins over "-i".
const ENDINGS: &[&str] = &[
    "ovi", "ami", "ách", "ach", "ích", "ich", "om", "ou", "ov", "ia", "ie", "iu", "ej", "ým", "a",
    "u", "e", "y", "i", "o", "á", "é", "í", "ý",
];

/// Lowercase function words (SK, CS, EN) that carry no retrieval signal.
const STOP_WORDS: &[&str] = &[
    // English
    "a", "an", "the", "my", "your", "our", "his", "her", "its", "their", "me", "i", "you", "he",
    "she", "it", "we", "they", "is", "are", "am", "was", "were", "be", "been", "to", "of", "in",
    "on", "at", "for", "with", "and", "or", "but", "not", "do", "does", "did", "this", "that",
    "these", "those", "as", "by", "from", // Slovak
    "aj", "ako", "ale", "alebo", "by", "bol", "bola", "bolo", "boli", "je", "jej", "jeho", "ja",
    "ty", "vy", "on", "ona", "ono", "oni", "som", "si", "sme", "ste", "sú", "sa", "so", "na", "v",
    "vo", "z", "zo", "do", "k", "ku", "od", "po", "pre", "pri", "o", "u", "s", "že", "to", "ten",
    "tá", "tie", "či", "len", "tiež", "mi", "mu", // Czech
    "jsem", "jsi", "jsme", "jste", "jsou", "se", "ve", "já", "nebo", "také", "mě", "ta", "tyto",
    "pro", "při", "za",
];

/// One term of a lexical query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryTerm {
    /// Lowercased word or stem; adapters fold diacritics themselves.
    pub text: String,
    /// Match as a prefix (`text*`) instead of the whole token.
    pub prefix: bool,
}

fn stem(word: &str) -> Option<String> {
    let chars = word.chars().count();
    if chars < STEM_MIN_WORD_CHARS {
        return None;
    }
    ENDINGS
        .iter()
        .filter(|ending| word.ends_with(**ending))
        .map(|ending| chars - ending.chars().count())
        .find(|kept| *kept >= STEM_MIN_CHARS)
        .map(|kept| word.chars().take(kept).collect())
}

impl LexicalQuery {
    /// The terms to search: every non-stop word, long ones also as prefixes,
    /// plus a stemmed prefix term for words that have one. Empty when the
    /// query has nothing but stop-words.
    pub fn terms(&self) -> Vec<QueryTerm> {
        let text: String = self.text.chars().take(MAX_QUERY_CHARS).collect();
        let mut terms: Vec<QueryTerm> = Vec::new();
        let mut push = |term: QueryTerm| {
            if !terms.contains(&term) {
                terms.push(term);
            }
        };
        let words = text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_lowercase)
            .filter(|word| !STOP_WORDS.contains(&word.as_str()))
            .take(MAX_QUERY_WORDS);
        for word in words {
            let prefixes = self.prefix_min_chars > 0;
            let long = prefixes && word.chars().count() >= self.prefix_min_chars as usize;
            let stemmed = if prefixes { stem(&word) } else { None };
            push(QueryTerm {
                text: word,
                prefix: long,
            });
            if let Some(stemmed) = stemmed {
                push(QueryTerm {
                    text: stemmed,
                    prefix: true,
                });
            }
        }
        terms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(query: &str) -> Vec<(String, bool)> {
        LexicalQuery::new(query)
            .terms()
            .into_iter()
            .map(|t| (t.text, t.prefix))
            .collect()
    }

    #[test]
    fn stems_longest_ending_first_keeping_four_chars() {
        assert_eq!(stem("veterinárovi").as_deref(), Some("veterinár"));
        assert_eq!(stem("veterinára").as_deref(), Some("veterinár"));
        assert_eq!(stem("kvetinami").as_deref(), Some("kvetin"));
    }

    #[test]
    fn short_words_and_short_stems_are_left_alone() {
        assert_eq!(stem("psom"), None);
        assert_eq!(stem("domov"), None, "stem would be three characters");
        assert_eq!(stem("rust"), None);
    }

    #[test]
    fn keeps_the_original_and_adds_the_stem() {
        assert_eq!(
            texts("Veterinára"),
            vec![
                ("veterinára".to_owned(), true),
                ("veterinár".to_owned(), true)
            ]
        );
    }

    #[test]
    fn stop_words_only_gives_no_terms() {
        assert!(texts("my is the").is_empty());
        assert!(texts("je som na v a").is_empty());
    }
}
