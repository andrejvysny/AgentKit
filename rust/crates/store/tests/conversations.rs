mod support;
use agentkit_store::*;
use serde_json::json;
use support::*;

#[tokio::test]
async fn branches_and_chain_appends_preserve_one_live_path() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    store
        .append_message(message("root", "chat", None), None, 1000)
        .await
        .unwrap();
    store
        .append_message(message("first", "chat", None), None, 1000)
        .await
        .unwrap();
    let mut branch = message("second", "chat", None);
    branch.parent_message_id = Some("root".into());
    store.append_message(branch, None, 1000).await.unwrap();
    let live = store.messages("chat", false).await.unwrap();
    assert_eq!(
        live.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        vec!["root", "second"]
    );
    let mut late = message("late", "chat", None);
    late.parent_message_id = Some("first".into());
    late.activate = false;
    assert!(!store.append_message(late, None, 1001).await.unwrap().active);
    let path = store.activate_path("first").await.unwrap();
    assert_eq!(path.last().unwrap().id, "late");
    let mut next = message("next", "chat", None);
    next.parent_message_id = Some("late".into());
    next.activate = false;
    assert!(store.append_message(next, None, 1001).await.unwrap().active);
    let mut sibling = message("sibling", "chat", None);
    sibling.parent_message_id = Some("late".into());
    sibling.activate = false;
    assert!(
        !store
            .append_message(sibling, None, 1001)
            .await
            .unwrap()
            .active
    );
    assert_eq!(
        store
            .messages("chat", false)
            .await
            .unwrap()
            .last()
            .unwrap()
            .id,
        "next"
    );
}

#[tokio::test]
async fn import_validates_before_writes_and_derives_tree_fields() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .create_chat("source", None, json!({}), 1000)
        .await
        .unwrap();
    let a = store
        .append_message(message("a", "source", None), None, 1000)
        .await
        .unwrap();
    let b = store
        .append_message(message("b", "source", None), None, 1000)
        .await
        .unwrap();
    let chat = ChatRecord {
        id: "imported".into(),
        title: None,
        metadata: json!({}),
        archived: false,
        created_at: 1000.into(),
        updated_at: 1000.into(),
    };
    let mut imported_a = a.clone();
    imported_a.id = "import-a".into();
    imported_a.order_key = 999;
    imported_a.depth = 999;
    let mut imported_b = b.clone();
    imported_b.id = "import-b".into();
    imported_b.parent_message_id = Some("missing".into());
    assert!(
        store
            .import_conversation(ImportConversation {
                chat: chat.clone(),
                messages: vec![imported_a.clone(), imported_b.clone()]
            })
            .await
            .is_err()
    );
    assert!(store.get_chat("imported").await.unwrap().is_none());
    imported_b.parent_message_id = Some("import-a".into());
    store
        .import_conversation(ImportConversation {
            chat,
            messages: vec![imported_a, imported_b],
        })
        .await
        .unwrap();
    let messages = store.messages("imported", true).await.unwrap();
    assert_eq!(messages[0].order_key, 1);
    assert_eq!(messages[0].depth, 0);
    assert_eq!(messages[1].depth, 1);
    let (fork, messages) = store
        .fork_chat("imported", "import-b", "fork", 1001)
        .await
        .unwrap();
    assert!(!fork.archived);
    assert_eq!(messages.len(), 2);
    assert_ne!(messages[0].id, "import-a");
}

#[tokio::test]
async fn search_includes_all_text_parts_and_inactive_branches_but_hides_private_records() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    let mut multi = message("multi", "chat", None);
    multi.content = json!([{"type":"text","text":"first"},{"type":"text","text":"quartz beacon"}]);
    store.append_message(multi, None, 1000).await.unwrap();
    let mut other = message("other", "chat", None);
    other.parent_message_id = Some("multi".into());
    other.content = json!("quartz beacon quartz beacon");
    store.append_message(other, None, 1000).await.unwrap();
    let mut private = message("private", "chat", None);
    private.content = json!("quartz beacon");
    private.metadata = json!({"internal":true});
    store.append_message(private, None, 1000).await.unwrap();
    let hits = store
        .search_messages("quartz beacon", None, 10)
        .await
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].message.id, "other");
    assert!(
        store
            .search_messages("***", None, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn search_answers_with_sqlite_fts5_semantics_not_substrings() {
    // The old predicate was a lowercased substring scan and drifted from the
    // FTS5 `MATCH` the TypeScript adapters answered with: substrings hit
    // inside words, diacritics missed, and multi-word queries demanded a
    // contiguous phrase. The store must answer the way SQLite's
    // `unicode61 remove_diacritics 2` index over `toFtsQuery` does.
    let store = SqliteStore::memory(4).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    let mut diacritic = message("diacritic", "chat", None);
    diacritic.content = json!("the résumé of the trip");
    store.append_message(diacritic, None, 1000).await.unwrap();
    let mut words = message("words", "chat", None);
    words.content = json!("quartz in the room and a beacon on the wall");
    store.append_message(words, None, 1000).await.unwrap();
    let mut server = message("server", "chat", None);
    server.content = json!("server side rendering");
    store.append_message(server, None, 1000).await.unwrap();
    let mut operators = message("operators", "chat", None);
    operators.content = json!("The c++ and (2) files");
    store.append_message(operators, None, 1000).await.unwrap();

    // Diacritic folding: "resume" finds "résumé" — the substring scan missed it.
    let hits = store.search_messages("resume", None, 10).await.unwrap();
    assert_eq!(
        hits.iter()
            .map(|hit| hit.message.id.as_str())
            .collect::<Vec<_>>(),
        ["diacritic"]
    );
    // Token matching, not substrings: "serv" must NOT hit "server".
    assert!(
        store
            .search_messages("serv", None, 10)
            .await
            .unwrap()
            .is_empty()
    );
    // Whole-word token does.
    let hits = store.search_messages("server", None, 10).await.unwrap();
    assert_eq!(hits[0].message.id, "server");
    // Multi-word is the implicit AND of tokens, not a contiguous phrase:
    // the words are far apart in `words` and still match.
    let hits = store
        .search_messages("quartz beacon", None, 10)
        .await
        .unwrap();
    assert_eq!(hits[0].message.id, "words");
    // FTS5 operators are data, not syntax: `(2)` searches for the token 2.
    let hits = store.search_messages("c++ (2)", None, 10).await.unwrap();
    assert_eq!(hits[0].message.id, "operators");
    // Nothing survives sanitizing → no hits, never an error.
    assert!(
        store
            .search_messages("***", None, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .search_messages("   ", None, 10)
            .await
            .unwrap()
            .is_empty()
    );
    // The chat filter still applies.
    assert!(
        store
            .search_messages("server", Some("missing-chat".into()), 10)
            .await
            .unwrap()
            .is_empty()
    );
}

fn hit_ids(hits: &[MessageSearchHit]) -> Vec<&str> {
    hits.iter().map(|hit| hit.message.id.as_str()).collect()
}

/// The persistent index answers exactly what SQLite's own FTS5 answers for
/// the same corpus and the same `toFtsQuery` output — every expectation
/// here is hand-computed against `unicode61 remove_diacritics 2`:
/// tokenization, not substring containment (the LIKE `%..%` class the old
/// predicate got wrong), diacritic folding on BOTH sides, operator
/// characters as data, and the CJK run as one token.
#[tokio::test]
async fn search_tokenization_matches_hand_computed_fts5() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    let doc = |id: &str, content: serde_json::Value| MessageInput {
        id: Some(id.into()),
        chat_id: "chat".into(),
        content,
        ..Default::default()
    };
    store
        .append_message(
            doc("diacritic", json!("the résumé of the trip")),
            None,
            1000,
        )
        .await
        .unwrap();
    store
        .append_message(
            doc(
                "words",
                json!("quartz in the room and a beacon on the wall"),
            ),
            None,
            1000,
        )
        .await
        .unwrap();
    store
        .append_message(doc("server", json!("server side rendering")), None, 1000)
        .await
        .unwrap();
    store
        .append_message(
            doc("compound", json!("server-side rendering is nice")),
            None,
            1000,
        )
        .await
        .unwrap();
    store
        .append_message(doc("operators", json!("The c++ and (2) files")), None, 1000)
        .await
        .unwrap();
    store
        .append_message(doc("cjk", json!("日本語のテキスト")), None, 1000)
        .await
        .unwrap();
    store
        .append_message(
            doc(
                "multi",
                json!([
                    {"type":"text","text":"first"},
                    {"type":"image","url":"x"},
                    {"type":"text","text":"quartz beacon"}
                ]),
            ),
            None,
            1000,
        )
        .await
        .unwrap();

    // THE divergence class the review reproduced: `LIKE '%serv%'` hits
    // inside words the tokenizer never produces. FTS5 must not return them.
    assert!(hit_ids(&store.search_messages("serv", None, 10).await.unwrap()).is_empty());
    // The whole token does — and unicode61 splits the hyphen, so a
    // "server-side" body carries the token "server" as well. The shorter
    // document wins bm25's length normalization: hand-computed [server,
    // compound] (scores 917478 > 759335 at the ×1e6 mirror scale).
    let hits = store.search_messages("server", None, 10).await.unwrap();
    assert_eq!(hit_ids(&hits), ["server", "compound"]);
    assert!(hits[0].score > hits[1].score);
    // Diacritic folding on both sides. The old hand-rolled score counted
    // the raw token "resume" inside lowercase "résumé" and scored 0; the
    // engine scores this hit 1412177.
    let hits = store.search_messages("resume", None, 10).await.unwrap();
    assert_eq!(hit_ids(&hits), ["diacritic"]);
    assert!(
        hits[0].score > 0,
        "bm25-derived score, not a substring count"
    );
    assert_eq!(
        hit_ids(&store.search_messages("résumé", None, 10).await.unwrap()),
        ["diacritic"]
    );
    // Operator characters are data: "(2)" tokenizes to "2", "c++" to "c",
    // and the phrase AND of the two scores higher than either alone
    // (2824354 > 1412177), all hand-computed postings of the same index.
    for query in ["2", "(2)", "c++", "c++ (2)", "c"] {
        assert_eq!(
            hit_ids(&store.search_messages(query, None, 10).await.unwrap()),
            ["operators"],
            "{query}"
        );
    }
    // unicode61 makes one CJK run one token: the full run matches, the
    // leading substring does not.
    assert_eq!(
        hit_ids(
            &store
                .search_messages("日本語のテキスト", None, 10)
                .await
                .unwrap()
        ),
        ["cjk"]
    );
    assert!(hit_ids(&store.search_messages("日本", None, 10).await.unwrap()).is_empty());
    // Parts bodies index ALL text parts joined by newlines (the image
    // part contributes nothing), and the short parts document outranks
    // the long prose one on bm25 length normalization (917478 > 530663).
    assert_eq!(
        hit_ids(
            &store
                .search_messages("quartz beacon", None, 10)
                .await
                .unwrap()
        ),
        ["multi", "words"]
    );
    // Scores follow the ranking: non-increasing along the hit order.
    let hits = store.search_messages("quartz", None, 10).await.unwrap();
    assert_eq!(hit_ids(&hits), ["multi", "words"]);
    assert!(hits[0].score >= hits[1].score);
    // The chat filter never turns a hit into a miss by accident of ranking.
    assert!(
        store
            .search_messages("server", Some("missing-chat".into()), 10)
            .await
            .unwrap()
            .is_empty()
    );
}

/// FTS5's query language stays data, never syntax: quotes, the negation
/// hyphen and the `OR` keyword go through `toFtsQuery` as literal text,
/// and the multi-word query is the implicit AND of separately tokenized
/// phrases — not a contiguous phrase, and not a boolean operator.
#[tokio::test]
async fn search_phrases_negation_and_keywords_are_literal_text() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    let doc = |id: &str, content: &str| MessageInput {
        id: Some(id.into()),
        chat_id: "chat".into(),
        content: json!(content),
        ..Default::default()
    };
    store
        .append_message(
            doc("far", "quartz far away on the other side but a beacon too"),
            None,
            1000,
        )
        .await
        .unwrap();
    store
        .append_message(doc("near", "quartz beacon"), None, 1000)
        .await
        .unwrap();
    store
        .append_message(doc("plainbeacon", "just a beacon here"), None, 1000)
        .await
        .unwrap();
    store
        .append_message(doc("logic", "quartz beacon or something"), None, 1000)
        .await
        .unwrap();

    // Typed with quotes or with the negation hyphen, the words are ANDed
    // as tokens: the far-apart document matches (NOT adjacency), the
    // beacon-only document never does, and the hand-computed bm25 order is
    // near(3) > logic(2) > far(1) at the ×1e6 mirror scale.
    for query in ["\"quartz beacon\"", "-quartz beacon"] {
        let hits = store.search_messages(query, None, 10).await.unwrap();
        assert_eq!(hit_ids(&hits), ["near", "logic", "far"], "{query}");
        assert!(hits[0].score > hits[1].score && hits[1].score > hits[2].score);
    }
    // `OR` is data: it only matches documents carrying the token "or".
    assert_eq!(
        hit_ids(
            &store
                .search_messages("quartz OR beacon", None, 10)
                .await
                .unwrap()
        ),
        ["logic"]
    );
    // A term no document carries answers empty — no error, no fallback.
    assert!(
        store
            .search_messages("quartz beacon only", None, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

/// The `unicode61` tokenizer keeps no stopword list: "the", "of" and "and"
/// are indexed tokens like any other. A stopword-ish query matches the
/// documents that carry those words and only those; a multi-stopword
/// query ANDs them.
#[tokio::test]
async fn search_indexes_stopword_ish_tokens() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    let doc = |id: &str, content: &str| MessageInput {
        id: Some(id.into()),
        chat_id: "chat".into(),
        content: json!(content),
        ..Default::default()
    };
    store
        .append_message(doc("withthe", "the theory of everything"), None, 1000)
        .await
        .unwrap();
    store
        .append_message(doc("without", "theory minus articles"), None, 1000)
        .await
        .unwrap();
    store
        .append_message(doc("anddoc", "this and that"), None, 1000)
        .await
        .unwrap();

    assert_eq!(
        hit_ids(&store.search_messages("the", None, 10).await.unwrap()),
        ["withthe"]
    );
    assert_eq!(
        hit_ids(&store.search_messages("of", None, 10).await.unwrap()),
        ["withthe"]
    );
    assert_eq!(
        hit_ids(&store.search_messages("and", None, 10).await.unwrap()),
        ["anddoc"]
    );
    // ANDed: no single document carries all three stopword tokens.
    assert!(
        store
            .search_messages("the of and", None, 10)
            .await
            .unwrap()
            .is_empty()
    );
    // Empty and whitespace-only queries exit before any scan; they are not
    // stopword queries and must never degrade into a full listing.
    for query in ["", "   ", "\t\n", "***", "()"] {
        assert!(
            store
                .search_messages(query, None, 10)
                .await
                .unwrap()
                .is_empty(),
            "{query:?}"
        );
    }
}

/// The index holds EVERY message; the chat/visibility constraints are
/// query-time filters over the join. A placeholder is invisible while it
/// is a placeholder and findable the moment its run projects the real
/// answer into the same row — a membership decision frozen at insert time
/// would keep that answer permanently unfindable. Inactive branches stay
/// searchable; archived, internal and placeholder records never surface.
#[tokio::test]
async fn search_applies_visibility_constraints_at_query_time() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    let mut placeholder = message("ph", "chat", None);
    placeholder.content = json!("fluxcapacitor placeholder body");
    placeholder.metadata = json!({"placeholder": true});
    store.append_message(placeholder, None, 1000).await.unwrap();
    assert!(
        store
            .search_messages("fluxcapacitor", None, 10)
            .await
            .unwrap()
            .is_empty()
    );
    // The run finishes: the SAME row (same rowid, same index entry) is
    // rewritten with the answer and no longer a placeholder.
    store
        .update_message(
            "ph",
            Some(json!("fluxcapacitor final answer")),
            Some(json!({})),
            None,
            None,
            1001,
        )
        .await
        .unwrap();
    assert_eq!(
        hit_ids(
            &store
                .search_messages("fluxcapacitor", None, 10)
                .await
                .unwrap()
        ),
        ["ph"]
    );

    let mut hidden = message("priv", "chat", None);
    hidden.content = json!("insider quartz note");
    hidden.metadata = json!({"internal": true});
    store.append_message(hidden, None, 1000).await.unwrap();
    assert!(
        store
            .search_messages("insider", None, 10)
            .await
            .unwrap()
            .is_empty()
    );
    store
        .update_message("priv", None, Some(json!({})), None, None, 1001)
        .await
        .unwrap();
    assert_eq!(
        hit_ids(&store.search_messages("insider", None, 10).await.unwrap()),
        ["priv"]
    );
    // Only a JSON `true` hides — the old predicate compared against
    // `Some(Value::Bool(true))`, and a numeric 1 keeps the row visible.
    let mut numeric = message("numone", "chat", None);
    numeric.content = json!("numone token here");
    numeric.metadata = json!({"internal": 1});
    store.append_message(numeric, None, 1000).await.unwrap();
    assert_eq!(
        hit_ids(&store.search_messages("numone", None, 10).await.unwrap()),
        ["numone"]
    );

    // An inactive branch is searchable; archived is not.
    let mut off = message("off", "chat", None);
    off.content = json!("branchemblem on the retired path");
    off.parent_message_id = Some("ph".into());
    off.activate = false;
    assert!(!store.append_message(off, None, 1000).await.unwrap().active);
    assert_eq!(
        hit_ids(
            &store
                .search_messages("branchemblem", None, 10)
                .await
                .unwrap()
        ),
        ["off"]
    );
    assert_eq!(store.archive_branch("off").await.unwrap(), 1);
    assert!(
        store
            .search_messages("branchemblem", None, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

/// The triggers — not any caller — keep the index in step. A deleted
/// message leaves no posting behind even when SQLite hands its rowid to
/// the very next insert (without the DELETE trigger the new message would
/// inherit "uniquealpha"); and a raw SQL INSERT, exactly the shape the
/// TypeScript importer writes, lands searchable through the INSERT
/// trigger, with the %_docsize shadow proving the durable index — not a
/// per-query reconstruction — holds the row.
#[tokio::test]
async fn search_triggers_cover_deletes_rowid_reuse_and_direct_inserts() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("triggers.db");
    {
        let store = SqliteStore::open(&path, 2).unwrap();
        store.create_chat("a", None, json!({}), 1000).await.unwrap();
        let mut first = message("m1", "a", None);
        first.content = json!("uniquealpha token");
        store.append_message(first, None, 1000).await.unwrap();
        assert_eq!(
            hit_ids(
                &store
                    .search_messages("uniquealpha", None, 10)
                    .await
                    .unwrap()
            ),
            ["m1"]
        );
        store.delete_chat("a").await.unwrap();
        store.create_chat("b", None, json!({}), 1000).await.unwrap();
        let mut second = message("m2", "b", None);
        second.content = json!("plainbeta token");
        store.append_message(second, None, 1000).await.unwrap();
        // The cascade deleted m1; m2 reused its rowid. The retired term
        // must be unfindable — postings followed the DELETE trigger.
        assert!(
            store
                .search_messages("uniquealpha", None, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            hit_ids(&store.search_messages("plainbeta", None, 10).await.unwrap()),
            ["m2"]
        );
    }
    // A direct SQL INSERT with no store code in sight — the TypeScript
    // importer's exact write shape — is indexed by the INSERT trigger.
    let raw = rusqlite::Connection::open(&path).unwrap();
    raw.execute(
        "INSERT INTO messages(id,chat_id,parent_id,order_key,record) VALUES(?,?,?,?,?)",
        rusqlite::params![
            "raw-1",
            "b",
            Option::<String>::None,
            7,
            json!({
                "id": "raw-1",
                "chatId": "b",
                "role": "user",
                "content": "zephyrdirect content",
                "metadata": {},
                "orderKey": 7,
                "depth": 1,
                "branchIndex": 0,
                "active": true,
                "createdAt": 1_761_134_400_000i64
            })
            .to_string()
        ],
    )
    .unwrap();
    let indexed: i64 = raw
        .query_row(
            "SELECT count(*) FROM message_search WHERE message_search MATCH '\"zephyrdirect\"'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(indexed, 1, "the INSERT trigger indexed the direct write");
    let stale: i64 = raw
        .query_row(
            "SELECT count(*) FROM message_search WHERE message_search MATCH '\"uniquealpha\"'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        stale, 0,
        "the DELETE trigger retired the cascade's postings"
    );
    drop(raw);
    let store = SqliteStore::open(&path, 2).unwrap();
    assert_eq!(
        hit_ids(
            &store
                .search_messages("zephyrdirect", None, 10)
                .await
                .unwrap()
        ),
        ["raw-1"]
    );
}

/// LIMIT over the ranked, query-time-filtered set, ordered by
/// (rank, rank_index, id) — a total order, so ties never shuffle between
/// calls and a page boundary never skips or repeats a hit.
#[tokio::test]
async fn search_orders_equal_scores_by_rank_index_then_id() {
    let store = SqliteStore::memory(4).unwrap();
    // Insert chatB's row FIRST so its rowid is lower while its id sorts
    // last: a rowid tie-break (the old ordering) would return z before a,
    // the (rank, rank_index, id) total order returns a before z.
    store
        .create_chat("chatB", None, json!({}), 1000)
        .await
        .unwrap();
    store
        .create_chat("chatA", None, json!({}), 1000)
        .await
        .unwrap();
    for (id, chat) in [("msg-z", "chatB"), ("msg-a", "chatA")] {
        let mut same = message(id, chat, None);
        same.content = json!("identical quartz phrase");
        store.append_message(same, None, 1000).await.unwrap();
    }
    let hits = store.search_messages("quartz", None, 10).await.unwrap();
    assert_eq!(hit_ids(&hits), ["msg-a", "msg-z"]);
    assert_eq!(hits[0].score, hits[1].score, "identical bodies tie in bm25");
    // Stable: the same query again answers the same order, and the LIMIT
    // page is exactly the prefix of the full ranking.
    let again = store.search_messages("quartz", None, 10).await.unwrap();
    assert_eq!(hit_ids(&again), hit_ids(&hits));
    assert_eq!(
        hit_ids(&store.search_messages("quartz", None, 1).await.unwrap()),
        ["msg-a"]
    );
    // Within one chat the rank_index (order_key) decides before id:
    // insertion-order 1,2,3 beats the alphabetical id order.
    store
        .create_chat("chatC", None, json!({}), 1000)
        .await
        .unwrap();
    for id in ["zz", "mm", "aa"] {
        let mut same = message(id, "chatC", None);
        same.content = json!("identical quartz phrase");
        store.append_message(same, None, 1000).await.unwrap();
    }
    assert_eq!(
        hit_ids(
            &store
                .search_messages("quartz", Some("chatC".into()), 10)
                .await
                .unwrap()
        ),
        ["zz", "mm", "aa"]
    );
    // A zero limit never pays a database round-trip and never a hit.
    assert!(
        store
            .search_messages("quartz", None, 0)
            .await
            .unwrap()
            .is_empty()
    );
}

/// Ranking is bm25's, not a substring count: equal raw-token occurrence
/// counts no longer tie a terse hit with a padded one — the longer
/// document loses to length normalization, in ORDER and in score
/// (hand-computed 3 > 2 at the ×1e6 mirror scale; the old substring
/// scoring said 2 == 2).
#[tokio::test]
async fn search_ranks_by_bm25_not_substring_counts() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    let doc = |id: &str, content: &str| MessageInput {
        id: Some(id.into()),
        chat_id: "chat".into(),
        content: json!(content),
        ..Default::default()
    };
    store
        .append_message(doc("short", "quartz beacon"), None, 1000)
        .await
        .unwrap();
    store
        .append_message(
            doc(
                "long",
                "quartz beacon plus ten fifteen twenty thirty forty fifty filler tokens",
            ),
            None,
            1000,
        )
        .await
        .unwrap();
    let hits = store
        .search_messages("quartz beacon", None, 10)
        .await
        .unwrap();
    assert_eq!(hit_ids(&hits), ["short", "long"]);
    assert!(hits[0].score > hits[1].score);
}

#[tokio::test]
async fn deletion_refuses_live_scope_and_cleans_durable_owned_state() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    store
        .submit_turn(task("run", "chat"), vec![message("a", "chat", Some("run"))])
        .await
        .unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    assert!(matches!(
        store.delete_chat("chat").await,
        Err(StoreError::ScopeBusy(_))
    ));
    finish(&store, lease, TaskStatus::Completed, 1001).await;
    store.delete_chat("chat").await.unwrap();
    assert!(store.get_task("run").await.unwrap().is_none());
    assert!(store.get_chat("chat").await.unwrap().is_none());
    assert!(store.outbox().await.unwrap().is_empty());
}

#[tokio::test]
async fn fork_refuses_inactive_branch_and_omits_placeholder_and_run_identity() {
    let store = SqliteStore::memory(2).unwrap();
    store
        .create_chat("chat", Some("title".into()), json!({}), 1000)
        .await
        .unwrap();
    store
        .append_message(message("root", "chat", Some("run")), None, 1000)
        .await
        .unwrap();
    store
        .append_message(message("old", "chat", Some("run")), None, 1000)
        .await
        .unwrap();
    let mut placeholder = message("placeholder", "chat", Some("run"));
    placeholder.parent_message_id = Some("root".into());
    placeholder.metadata = json!({"placeholder":true});
    store.append_message(placeholder, None, 1000).await.unwrap();
    assert!(
        matches!(store.fork_chat("chat","old","bad",1001).await,Err(StoreError::TaskInput { reason: agentkit_engine::TaskInputReason::InvalidForkPoint, message }) if message=="invalid_fork_point")
    );
    let (chat, messages) = store
        .fork_chat("chat", "placeholder", "copy", 1001)
        .await
        .unwrap();
    assert_eq!(chat.title.as_deref(), Some("Fork of title"));
    assert_eq!(messages.len(), 1);
    assert!(messages[0].run_id.is_none());
    assert!(messages[0].metadata.get("placeholder").is_none());
    store.submit(task("queued", "chat")).await.unwrap();
    assert!(matches!(
        store.delete_chat("chat").await,
        Err(StoreError::ScopeBusy(_))
    ));
}

#[tokio::test]
async fn provider_order_keeps_user_before_each_linked_tool_pass() {
    let store = SqliteStore::memory(2).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    store
        .append_message(message("user", "chat", Some("run")), None, 1000)
        .await
        .unwrap();
    let mut answer = message("answer", "chat", Some("run"));
    answer.role = "assistant".into();
    store.append_message(answer, None, 1000).await.unwrap();
    for (name, call) in [("internal1", "call1"), ("internal2", "call2")] {
        let mut assistant = message(name, "chat", Some("run"));
        assistant.role = "assistant".into();
        assistant.metadata = json!({"internal":true});
        assistant.tool_calls = Some(json!([{"id":call}]));
        store.append_message(assistant, None, 1000).await.unwrap();
    }
    for (name, call) in [("result2", "call2"), ("result1", "call1")] {
        let mut result = message(name, "chat", Some("run"));
        result.role = "tool".into();
        result.tool_call_id = Some(call.into());
        store.append_message(result, None, 1000).await.unwrap();
    }
    let records = order_messages_for_provider(store.messages("chat", false).await.unwrap());
    assert_eq!(
        records.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        vec![
            "user",
            "internal1",
            "result1",
            "internal2",
            "result2",
            "answer"
        ]
    );
}
