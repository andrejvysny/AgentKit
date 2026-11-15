//! Toolset recommender (native tool use T4.4, TU-29).
//!
//! Ranks the session's toolsets against the user's request and writes ONE
//! line into the per-turn, user-side context (never the cached prefix, S1-S4).
//! It only advises: no toolset is removed from the catalog or the tools array.
//! Lexical match comes first, usage statistics second. A vector term plugs in
//! later (roadmap 3.5) through [`ToolsetScorer`].
use crate::ToolDefinition;
use crate::toolset::{META_TOOLSET, Promotions, ToolsetView};
use agentkit_contracts::{AiRunToolRecordEventData, AiToolOutcomeClass, Optional};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

/// PROPOSAL (not agreed): share of labelled requests whose expected toolset
/// must be in the top two. Recorded for TU-29; X7 may move it.
pub const TOP_TWO_HIT_RATE_THRESHOLD: f64 = 0.8;
/// How many toolsets the line names.
pub const DEFAULT_TOP_K: usize = 2;
/// Combined scores below this are not worth a line.
pub const MIN_SCORE: f64 = 0.05;
/// Weight of the usage prior next to the lexical score (weight 1.0).
pub const USAGE_WEIGHT: f64 = 0.15;
/// The top toolset is auto-promoted only above this score (promote option).
pub const PROMOTE_MIN_SCORE: f64 = 0.4;

const FIELD_ID: f64 = 3.0;
const FIELD_KEYWORD: f64 = 3.0;
const FIELD_TOOL_NAME: f64 = 2.0;
const FIELD_CARD: f64 = 2.0;
const FIELD_DESCRIPTION: f64 = 1.0;
const FIELD_GUIDE: f64 = 0.5;
const MAX_FIELD: f64 = 3.0;

/// What the recommender knows about one toolset.
#[derive(Clone, Debug, Default)]
pub struct ToolsetDoc {
    pub id: String,
    pub title: String,
    pub card: String,
    /// (offered tool name, description).
    pub tools: Vec<(String, String)>,
    pub guide: String,
    /// Host aliases, e.g. Slovak words for an English toolset.
    pub keywords: Vec<String>,
}

impl ToolsetDoc {
    pub fn with_keywords(mut self, keywords: &[&str]) -> Self {
        self.keywords = keywords.iter().map(|word| (*word).to_owned()).collect();
        self
    }
}

/// Docs for every toolset of a session view, including dynamic ones that
/// only appear through definitions. The meta toolset is left out.
pub fn docs_from_view(view: &ToolsetView, definitions: &[ToolDefinition]) -> Vec<ToolsetDoc> {
    let mut by_id: BTreeMap<String, ToolsetDoc> = BTreeMap::new();
    for toolset in view.toolsets().filter(|set| set.id != META_TOOLSET) {
        let mut guide = toolset
            .guide
            .as_ref()
            .map(|g| g.text.clone())
            .unwrap_or_default();
        for (_, topic) in &toolset.topics {
            guide.push('\n');
            guide.push_str(&topic.text);
        }
        by_id.insert(
            toolset.id.clone(),
            ToolsetDoc {
                id: toolset.id.clone(),
                title: toolset.title.clone(),
                card: toolset.card.clone(),
                tools: toolset
                    .tools
                    .iter()
                    .map(|id| (id.clone(), String::new()))
                    .collect(),
                guide,
                keywords: vec![],
            },
        );
    }
    for definition in definitions {
        let id = view.id_of(definition);
        if id == META_TOOLSET {
            continue;
        }
        let doc = by_id.entry(id.clone()).or_insert_with(|| ToolsetDoc {
            id: id.clone(),
            title: id.clone(),
            ..ToolsetDoc::default()
        });
        match doc
            .tools
            .iter_mut()
            .find(|(name, _)| *name == definition.name)
        {
            Some(tool) => tool.1 = definition.description.clone(),
            None => doc
                .tools
                .push((definition.name.clone(), definition.description.clone())),
        }
    }
    by_id.into_values().collect()
}

fn fold(c: char) -> char {
    match c {
        'á' | 'ä' | 'â' | 'à' | 'ã' | 'å' => 'a',
        'č' | 'ć' | 'ç' => 'c',
        'ď' => 'd',
        'é' | 'ě' | 'ë' | 'è' | 'ê' => 'e',
        'í' | 'î' | 'ï' | 'ì' => 'i',
        'ĺ' | 'ľ' | 'ł' => 'l',
        'ň' | 'ń' | 'ñ' => 'n',
        'ó' | 'ô' | 'ö' | 'ò' | 'õ' => 'o',
        'ŕ' | 'ř' => 'r',
        'š' | 'ś' => 's',
        'ť' | 'ţ' => 't',
        'ú' | 'ů' | 'ü' | 'ù' | 'û' => 'u',
        'ý' | 'ÿ' => 'y',
        'ž' | 'ź' | 'ż' => 'z',
        other => other,
    }
}

const STOPWORDS: &[&str] = &[
    "the",
    "and",
    "for",
    "with",
    "please",
    "this",
    "that",
    "from",
    "can",
    "you",
    "your",
    "how",
    "what",
    "are",
    "was",
    "into",
    "all",
    "any",
    "mi",
    "mam",
    "ako",
    "co",
    "pre",
    "pri",
    "sa",
    "je",
    "som",
    "ale",
    "aby",
    "chcem",
    "potrebujem",
    "prosim",
    "moj",
    "moje",
    "to",
    "tu",
];

/// Lower-case, diacritics-folded word tokens (Slovak and other Latin
/// scripts), without stop words, one trailing plural "s" removed.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = vec![];
    let mut current = String::new();
    let flush = |current: &mut String, out: &mut Vec<String>| {
        if !current.is_empty() {
            let mut word = std::mem::take(current);
            if word.len() > 3 && word.ends_with('s') && !word.ends_with("ss") {
                word.pop();
            }
            if word.len() >= 2 && !STOPWORDS.contains(&word.as_str()) {
                out.push(word);
            }
        }
    };
    for c in text.chars().flat_map(char::to_lowercase) {
        let c = fold(c);
        if c.is_alphanumeric() {
            current.push(c);
        } else {
            flush(&mut current, &mut out);
        }
    }
    flush(&mut current, &mut out);
    out
}

/// Equal, or sharing a long prefix (covers Slovak/English inflection).
fn same_word(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let shortest = a.len().min(b.len());
    if shortest < 5 {
        return false;
    }
    let prefix = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    prefix >= 5usize.max(shortest - 2)
}

/// How a scorer reads the request: as relevance, or as a prior that only
/// reorders toolsets that are already relevant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScoreRole {
    Relevance,
    Prior,
}

/// The seam for scoring terms. `score` returns one value in `[0, 1]` per doc,
/// in `docs` order. A vector scorer (roadmap 3.5) implements this too.
pub trait ToolsetScorer: Send + Sync {
    fn role(&self) -> ScoreRole {
        ScoreRole::Relevance
    }
    fn score(&self, request: &str, docs: &[ToolsetDoc]) -> Vec<f64>;
}

/// Word match of the request against id, title, keywords, tool names, card,
/// tool descriptions and guide, rarer words counting more.
#[derive(Default)]
pub struct LexicalScorer;

fn fields(doc: &ToolsetDoc) -> Vec<(f64, Vec<String>)> {
    let tool_names: Vec<String> = doc
        .tools
        .iter()
        .flat_map(|(name, _)| tokenize(name))
        .collect();
    let descriptions: Vec<String> = doc
        .tools
        .iter()
        .flat_map(|(_, text)| tokenize(text))
        .collect();
    vec![
        (FIELD_ID, tokenize(&format!("{} {}", doc.id, doc.title))),
        (
            FIELD_KEYWORD,
            doc.keywords.iter().flat_map(|k| tokenize(k)).collect(),
        ),
        (FIELD_TOOL_NAME, tool_names),
        (FIELD_CARD, tokenize(&doc.card)),
        (FIELD_DESCRIPTION, descriptions),
        (FIELD_GUIDE, tokenize(&doc.guide)),
    ]
}

impl ToolsetScorer for LexicalScorer {
    fn score(&self, request: &str, docs: &[ToolsetDoc]) -> Vec<f64> {
        let indexed: Vec<_> = docs.iter().map(fields).collect();
        let words: BTreeSet<String> = tokenize(request).into_iter().collect();
        let mut numerator = vec![0.0; docs.len()];
        let mut denominator = 0.0;
        for word in &words {
            let hits: Vec<f64> = indexed
                .iter()
                .map(|doc| {
                    doc.iter()
                        .filter(|(_, tokens)| tokens.iter().any(|t| same_word(word, t)))
                        .map(|(weight, _)| *weight)
                        .fold(0.0, f64::max)
                })
                .collect();
            let matching = hits.iter().filter(|hit| **hit > 0.0).count();
            if matching == 0 {
                continue;
            }
            let rarity = 1.0 + (docs.len() as f64 / matching as f64).ln();
            denominator += rarity * MAX_FIELD;
            for (sum, hit) in numerator.iter_mut().zip(&hits) {
                *sum += rarity * hit;
            }
        }
        if denominator == 0.0 {
            return numerator;
        }
        numerator.iter().map(|sum| sum / denominator).collect()
    }
}

/// Port for usage counts (ok calls per toolset). Hosts persist it; the
/// in-memory store below is the default. `scope` is a session or workspace.
pub trait UsageStore: Send + Sync {
    fn record_ok(&self, scope: &str, toolset: &str);
    fn counts(&self, scope: &str) -> BTreeMap<String, u64>;
}

#[derive(Default)]
pub struct InMemoryUsage {
    counts: Mutex<BTreeMap<(String, String), u64>>,
}

impl UsageStore for InMemoryUsage {
    fn record_ok(&self, scope: &str, toolset: &str) {
        let mut counts = self.counts.lock().unwrap_or_else(|p| p.into_inner());
        *counts
            .entry((scope.to_owned(), toolset.to_owned()))
            .or_insert(0) += 1;
    }

    fn counts(&self, scope: &str) -> BTreeMap<String, u64> {
        let counts = self.counts.lock().unwrap_or_else(|p| p.into_inner());
        counts
            .iter()
            .filter(|((owner, _), _)| owner == scope)
            .map(|((_, toolset), count)| (toolset.clone(), *count))
            .collect()
    }
}

/// Counts one `run.tool.record` event (T0.1) as usage when the call
/// succeeded and belongs to a non-meta toolset. Returns whether it counted.
pub fn record_usage(
    store: &dyn UsageStore,
    scope: &str,
    record: &AiRunToolRecordEventData,
) -> bool {
    let succeeded = matches!(
        record.outcome_class,
        AiToolOutcomeClass::Ok | AiToolOutcomeClass::OkAfterRepair
    );
    match (&record.toolset, succeeded) {
        (Optional::Present(toolset), true) if toolset != META_TOOLSET => {
            store.record_ok(scope, toolset);
            true
        }
        _ => false,
    }
}

/// Log-scaled share of the busiest toolset; independent of the request.
pub struct UsageScorer {
    store: Arc<dyn UsageStore>,
    scope: String,
}

impl UsageScorer {
    pub fn new(store: Arc<dyn UsageStore>, scope: &str) -> Self {
        Self {
            store,
            scope: scope.to_owned(),
        }
    }
}

impl ToolsetScorer for UsageScorer {
    fn role(&self) -> ScoreRole {
        ScoreRole::Prior
    }

    fn score(&self, _request: &str, docs: &[ToolsetDoc]) -> Vec<f64> {
        let counts = self.store.counts(&self.scope);
        let busiest = counts.values().copied().max().unwrap_or(0);
        docs.iter()
            .map(|doc| match (counts.get(&doc.id), busiest) {
                (Some(count), max) if max > 0 => {
                    (1.0 + *count as f64).ln() / (1.0 + max as f64).ln()
                }
                _ => 0.0,
            })
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct RecommenderOptions {
    pub top_k: usize,
    pub min_score: f64,
    /// Promote-mode profiles: auto-load the top toolset at task start.
    /// Default off.
    pub promote_top: bool,
    pub promote_min_score: f64,
}

impl Default for RecommenderOptions {
    fn default() -> Self {
        Self {
            top_k: DEFAULT_TOP_K,
            min_score: MIN_SCORE,
            promote_top: false,
            promote_min_score: PROMOTE_MIN_SCORE,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Scored {
    pub toolset: String,
    pub score: f64,
}

/// The advice for one turn.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Recommendation {
    pub top: Vec<Scored>,
    /// The toolset to promote at task start; only with `promote_top`.
    pub promote: Option<String>,
}

impl Recommendation {
    /// The one line for the turn context, or None when nothing is relevant.
    pub fn line(&self) -> Option<String> {
        if self.top.is_empty() {
            return None;
        }
        let names: Vec<&str> = self.top.iter().map(|s| s.toolset.as_str()).collect();
        Some(format!(
            "Likely useful toolsets: {} (load with toolset_load)",
            names.join(", ")
        ))
    }

    /// Appends the line to per-turn, user-side context text.
    pub fn append_to(&self, context: &mut String) {
        if let Some(line) = self.line() {
            if !context.is_empty() && !context.ends_with('\n') {
                context.push('\n');
            }
            context.push_str(&line);
        }
    }

    /// Asks for the promotion at the next turn boundary. False when there is
    /// none or it was already requested.
    pub fn request_promotion(&self, promotions: &Promotions) -> bool {
        self.promote
            .as_deref()
            .is_some_and(|id| promotions.request(id))
    }
}

pub struct Recommender {
    docs: Vec<ToolsetDoc>,
    scorers: Vec<(Arc<dyn ToolsetScorer>, f64)>,
    options: RecommenderOptions,
}

impl Recommender {
    /// Lexical scoring only.
    pub fn new(docs: Vec<ToolsetDoc>) -> Self {
        Self {
            docs,
            scorers: vec![(Arc::new(LexicalScorer), 1.0)],
            options: RecommenderOptions::default(),
        }
    }

    pub fn with_usage(self, store: Arc<dyn UsageStore>, scope: &str) -> Self {
        self.with_scorer(Arc::new(UsageScorer::new(store, scope)), USAGE_WEIGHT)
    }

    /// Adds a term, e.g. the vector scorer later.
    pub fn with_scorer(mut self, scorer: Arc<dyn ToolsetScorer>, weight: f64) -> Self {
        self.scorers.push((scorer, weight));
        self
    }

    pub fn with_options(mut self, options: RecommenderOptions) -> Self {
        self.options = options;
        self
    }

    /// Every toolset, best first. Never drops one: ranking is advice only.
    pub fn rank(&self, request: &str) -> Vec<Scored> {
        let mut relevance = vec![0.0; self.docs.len()];
        let mut prior = vec![0.0; self.docs.len()];
        for (scorer, weight) in &self.scorers {
            let target = match scorer.role() {
                ScoreRole::Relevance => &mut relevance,
                ScoreRole::Prior => &mut prior,
            };
            for (sum, value) in target.iter_mut().zip(scorer.score(request, &self.docs)) {
                *sum += weight * value;
            }
        }
        let mut ranked: Vec<Scored> = self
            .docs
            .iter()
            .enumerate()
            .map(|(at, doc)| Scored {
                toolset: doc.id.clone(),
                // A prior reorders relevant toolsets; it never recommends alone.
                score: relevance[at] + if relevance[at] > 0.0 { prior[at] } else { 0.0 },
            })
            .collect();
        ranked.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.toolset.cmp(&b.toolset))
        });
        ranked
    }

    /// The advice for one turn; `loaded` toolsets are not suggested again.
    pub fn recommend(&self, request: &str, loaded: &[String]) -> Recommendation {
        let top: Vec<Scored> = self
            .rank(request)
            .into_iter()
            .filter(|s| s.score >= self.options.min_score && !loaded.contains(&s.toolset))
            .take(self.options.top_k)
            .collect();
        let promote = top
            .first()
            .filter(|s| self.options.promote_top && s.score >= self.options.promote_min_score)
            .map(|s| s.toolset.clone());
        Recommendation { top, promote }
    }
}
