//! In-memory `RetrievalIndex`: whole-word and prefix lexical matching with
//! case and Latin-diacritic folding (the FTS5 `unicode61 remove_diacritics`
//! behaviour the SQLite adapter provides) and brute-force cosine kNN.
//! Transactions are optimistic copies, as in [`crate::InMemoryMemoryRepository`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use agentkit_memory::{
    Corpus, Hit, IndexFilter, IndexItem, IndexStats, IndexTx, ItemId, LexicalQuery, MemoryError,
    MemoryResult, ModelId, ModelSpec, ModelState, ModelStats, PurgeReport, RetrievalIndex,
    Searchability, Timestamp,
};
use async_trait::async_trait;

#[derive(Clone)]
struct Item {
    corpus: Corpus,
    scope_key: String,
    ts: Timestamp,
    searchable: Searchability,
    sha: [u8; 32],
    tokens: Vec<String>,
    vectors: BTreeMap<ModelId, Vec<f32>>,
}

impl Item {
    fn matches(&self, filter: &IndexFilter) -> bool {
        (filter.corpora.is_empty() || filter.corpora.contains(&self.corpus))
            && (filter.scope_keys.is_empty() || filter.scope_keys.contains(&self.scope_key))
            && (filter.include_superseded || self.searchable == Searchability::Current)
            && filter.since.is_none_or(|since| self.ts >= since)
            && filter.until.is_none_or(|until| self.ts <= until)
    }
}

#[derive(Default, Clone)]
struct ItemTable(BTreeMap<ItemId, Arc<Item>>);

#[derive(Clone, Default)]
struct State {
    version: u64,
    items: ItemTable,
    models: BTreeMap<ModelId, (ModelSpec, ModelState)>,
}

struct Staged {
    base: u64,
    state: State,
}

/// Clones share one committed state.
#[derive(Clone, Default)]
pub struct InMemoryRetrievalIndex {
    state: Arc<Mutex<State>>,
}

impl InMemoryRetrievalIndex {
    pub fn new() -> Self {
        Self::default()
    }

    fn committed(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("retrieval index state")
    }
}

fn fold_char(c: char) -> char {
    match c {
        'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'ā' => 'a',
        'č' | 'ć' | 'ç' => 'c',
        'ď' => 'd',
        'é' | 'è' | 'ê' | 'ë' | 'ě' | 'ē' => 'e',
        'í' | 'ì' | 'î' | 'ï' => 'i',
        'ĺ' | 'ľ' | 'ł' => 'l',
        'ň' | 'ñ' | 'ń' => 'n',
        'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ø' => 'o',
        'ŕ' | 'ř' => 'r',
        'š' | 'ś' => 's',
        'ť' => 't',
        'ú' | 'ù' | 'û' | 'ü' | 'ů' => 'u',
        'ý' | 'ÿ' => 'y',
        'ž' | 'ź' | 'ż' => 'z',
        other => other,
    }
}

fn tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .chars()
        .map(fold_char)
        .collect::<String>()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
    let denom = norm(a) * norm(b);
    if denom == 0.0 { 0.0 } else { dot / denom }
}

fn best_first(mut hits: Vec<Hit>, k: u32) -> Vec<Hit> {
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    hits.truncate(k as usize);
    hits
}

fn staged(tx: &mut IndexTx) -> MemoryResult<&mut Staged> {
    tx.state_mut::<Staged>()
}

#[async_trait]
impl RetrievalIndex for InMemoryRetrievalIndex {
    async fn begin_index(&self) -> MemoryResult<IndexTx> {
        let state = self.committed().clone();
        Ok(IndexTx::new(Staged {
            base: state.version,
            state,
        }))
    }

    async fn commit_index(&self, tx: IndexTx) -> MemoryResult<()> {
        let staged = tx.into_state::<Staged>()?;
        let mut committed = self.committed();
        if committed.version != staged.base {
            return Err(MemoryError::Conflict(
                "another commit moved the index".to_owned(),
            ));
        }
        *committed = staged.state;
        committed.version += 1;
        Ok(())
    }

    async fn upsert(&self, tx: &mut IndexTx, items: &[IndexItem]) -> MemoryResult<()> {
        let state = &mut staged(tx)?.state;
        for item in items {
            let mut vectors = match state.items.0.get(&item.id) {
                Some(existing) if existing.sha == item.text_sha256 => existing.vectors.clone(),
                _ => BTreeMap::new(),
            };
            if let Some((model, vector)) = &item.vector {
                let (spec, _) = state
                    .models
                    .get(model)
                    .ok_or_else(|| MemoryError::UnknownModel(model.clone()))?;
                if vector.len() != spec.dims as usize {
                    return Err(MemoryError::InvalidInput(format!(
                        "vector has {} dims, model {model} has {}",
                        vector.len(),
                        spec.dims
                    )));
                }
                vectors.insert(model.clone(), vector.clone());
            }
            state.items.0.insert(
                item.id.clone(),
                Arc::new(Item {
                    corpus: item.corpus,
                    scope_key: item.scope_key.clone(),
                    ts: item.ts,
                    searchable: item.searchable,
                    sha: item.text_sha256,
                    tokens: tokens(&item.text),
                    vectors,
                }),
            );
        }
        Ok(())
    }

    async fn delete(&self, tx: &mut IndexTx, ids: &[ItemId]) -> MemoryResult<()> {
        let state = &mut staged(tx)?.state;
        for id in ids {
            state.items.0.remove(id);
        }
        Ok(())
    }

    async fn lexical(&self, q: &LexicalQuery, f: &IndexFilter, k: u32) -> MemoryResult<Vec<Hit>> {
        let mut terms = tokens(&q.text);
        terms.sort();
        terms.dedup();
        let committed = self.committed();
        let mut hits = Vec::new();
        for (id, item) in &committed.items.0 {
            if !item.matches(f) {
                continue;
            }
            let score: f32 = terms
                .iter()
                .map(|term| {
                    if item.tokens.iter().any(|word| word == term) {
                        1.0
                    } else if q.prefix_min_chars > 0
                        && term.chars().count() >= q.prefix_min_chars as usize
                        && item
                            .tokens
                            .iter()
                            .any(|word| word.starts_with(term.as_str()))
                    {
                        0.5
                    } else {
                        0.0
                    }
                })
                .sum();
            if score > 0.0 {
                hits.push(Hit {
                    id: id.clone(),
                    corpus: item.corpus,
                    score,
                });
            }
        }
        Ok(best_first(hits, k))
    }

    async fn knn(
        &self,
        model: &ModelId,
        v: &[f32],
        f: &IndexFilter,
        k: u32,
    ) -> MemoryResult<Vec<Hit>> {
        let committed = self.committed();
        let (spec, _) = committed
            .models
            .get(model)
            .ok_or_else(|| MemoryError::UnknownModel(model.clone()))?;
        if v.len() != spec.dims as usize {
            return Err(MemoryError::InvalidInput(format!(
                "query has {} dims, model {model} has {}",
                v.len(),
                spec.dims
            )));
        }
        let hits = committed
            .items
            .0
            .iter()
            .filter(|(_, item)| item.matches(f))
            .filter_map(|(id, item)| {
                item.vectors.get(model).map(|vector| Hit {
                    id: id.clone(),
                    corpus: item.corpus,
                    score: cosine(v, vector),
                })
            })
            .collect();
        Ok(best_first(hits, k))
    }

    async fn begin_reembed(&self, model: &ModelSpec) -> MemoryResult<()> {
        let mut committed = self.committed();
        match committed.models.get(&model.id) {
            Some((spec, ModelState::Building)) if spec == model => return Ok(()),
            Some(_) => {
                return Err(MemoryError::InvalidInput(format!(
                    "model {} is already registered",
                    model.id
                )));
            }
            None => {}
        }
        committed
            .models
            .insert(model.id.clone(), (model.clone(), ModelState::Building));
        committed.version += 1;
        Ok(())
    }

    async fn commit_reembed(&self, model: &ModelId) -> MemoryResult<()> {
        let mut committed = self.committed();
        match committed.models.get(model) {
            None => return Err(MemoryError::UnknownModel(model.clone())),
            Some((_, state)) if *state != ModelState::Building => {
                return Err(MemoryError::InvalidInput(format!(
                    "model {model} is not building"
                )));
            }
            Some(_) => {}
        }
        for (_, state) in committed.models.values_mut() {
            if *state == ModelState::Active {
                *state = ModelState::Retired;
            }
        }
        if let Some((_, state)) = committed.models.get_mut(model) {
            *state = ModelState::Active;
        }
        committed.version += 1;
        Ok(())
    }

    async fn drop_model(&self, model: &ModelId) -> MemoryResult<()> {
        let mut committed = self.committed();
        match committed.models.get(model) {
            None => return Err(MemoryError::UnknownModel(model.clone())),
            Some((_, ModelState::Active)) => {
                return Err(MemoryError::InvalidInput(format!(
                    "model {model} is active and cannot be dropped"
                )));
            }
            Some(_) => {}
        }
        committed.models.remove(model);
        for item in committed.items.0.values_mut() {
            if item.vectors.contains_key(model) {
                Arc::make_mut(item).vectors.remove(model);
            }
        }
        committed.version += 1;
        Ok(())
    }

    async fn purge(&self, ids: &[ItemId]) -> MemoryResult<PurgeReport> {
        let mut committed = self.committed();
        let mut report = PurgeReport::default();
        for id in ids {
            if let Some(item) = committed.items.0.remove(id) {
                *report
                    .rows_removed
                    .entry("index_items".to_owned())
                    .or_default() += 1;
                *report
                    .rows_removed
                    .entry("index_vectors".to_owned())
                    .or_default() += item.vectors.len() as u64;
            }
        }
        report.residue = ids
            .iter()
            .filter(|id| committed.items.0.contains_key(*id))
            .count() as u64;
        committed.version += 1;
        Ok(report)
    }

    async fn stats(&self) -> MemoryResult<IndexStats> {
        let committed = self.committed();
        let models = committed
            .models
            .iter()
            .map(|(id, (spec, state))| ModelStats {
                id: id.clone(),
                dims: spec.dims,
                state: *state,
                vectors: committed
                    .items
                    .0
                    .values()
                    .filter(|item| item.vectors.contains_key(id))
                    .count() as u64,
            })
            .collect();
        Ok(IndexStats {
            items: committed.items.0.len() as u64,
            models,
        })
    }
}
