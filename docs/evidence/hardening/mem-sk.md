# hard(mem-sk): Slovak-aware lexical memory search

Problem: "k veterinárovi MVDr. Rahalqui" not found by "veterinára" (prefix match fails on SK/CS inflection).

Fix (query side, no index change): `LexicalQuery::terms()` in `agentkit-memory` (`lexical_terms.rs`), used by the SQLite FTS5 adapter and the testkit twin.
- Words of 5+ chars also yield a stemmed prefix term (longest ending first, stem >= 4 chars); original term kept.
- SK/CS/EN stop-words dropped; stop-word-only query returns nothing.
- Stems only when prefix matching is on (`prefix_min_chars > 0`).

Tests (conformance, run on twin and SQLite): `lexical_stems_slovak_inflection`, `lexical_drops_stop_words`; unit tests in `lexical_terms.rs`.
Failing before: `sqlite_retrieval_index_conforms` ... `lexical_stems_slovak_inflection` failed: left: [] right: ["vet"].

Live eval (`mem-retrieval-sk`) not run: provider policy forbids live calls. No offline retrieval-score set exists in the repo.
