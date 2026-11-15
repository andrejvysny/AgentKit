//! Toolset recommender (T4.4, TU-29): on labelled requests the right toolset
//! is in the top two at the agreed rate, and no toolset is ever hidden.
use agentkit_contracts::AiRunToolRecordEventData;
use agentkit_core::*;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Deserialize)]
struct Tool {
    name: String,
    description: String,
}

#[derive(Deserialize)]
struct Set {
    id: String,
    title: String,
    card: String,
    tools: Vec<Tool>,
    keywords: Vec<String>,
}

#[derive(Deserialize)]
struct Labelled {
    text: String,
    expect: String,
}

#[derive(Deserialize)]
struct Eval {
    toolsets: Vec<Set>,
    requests: Vec<Labelled>,
}

fn eval() -> Eval {
    serde_json::from_str(include_str!("fixtures/recommender-eval.json")).expect("fixture")
}

fn docs(eval: &Eval) -> Vec<ToolsetDoc> {
    eval.toolsets
        .iter()
        .map(|set| ToolsetDoc {
            id: set.id.clone(),
            title: set.title.clone(),
            card: set.card.clone(),
            tools: set
                .tools
                .iter()
                .map(|t| (t.name.clone(), t.description.clone()))
                .collect(),
            guide: String::new(),
            keywords: set.keywords.clone(),
        })
        .collect()
}

#[test]
fn labelled_requests_hit_top_two_at_the_agreed_rate() {
    let eval = eval();
    assert!(eval.requests.len() >= 30);
    assert!(
        eval.requests.iter().any(|r| !r.text.is_ascii()),
        "Slovak requests present"
    );
    let recommender = Recommender::new(docs(&eval));
    let mut misses = vec![];
    for request in &eval.requests {
        let top: Vec<String> = recommender
            .rank(&request.text)
            .into_iter()
            .take(2)
            .map(|s| s.toolset)
            .collect();
        if !top.contains(&request.expect) {
            misses.push(format!(
                "{} -> {top:?}, wanted {}",
                request.text, request.expect
            ));
        }
    }
    let rate = 1.0 - misses.len() as f64 / eval.requests.len() as f64;
    assert!(
        rate >= TOP_TWO_HIT_RATE_THRESHOLD,
        "hit rate {rate}: {misses:#?}"
    );
}

#[test]
fn tokenizer_folds_slovak_diacritics() {
    assert_eq!(
        tokenize_request("Prečítaj SÚBOR žltý"),
        ["precitaj", "subor", "zlty"]
    );
}

#[test]
fn line_is_one_line_and_appends_to_turn_context() {
    let recommender = Recommender::new(docs(&eval()));
    let advice = recommender.recommend("Run the unit tests in the shell", &[]);
    let line = advice.line().expect("line");
    assert!(line.starts_with("Likely useful toolsets: shell"));
    assert!(line.ends_with("(load with toolset_load)"));
    assert!(!line.contains('\n'));
    let mut context = String::from("User asked: run tests");
    advice.append_to(&mut context);
    assert_eq!(context.lines().count(), 2);
    assert!(recommender.recommend("zzz qqq", &[]).line().is_none());
    let loaded = vec!["shell".to_owned()];
    assert!(
        !recommender
            .recommend("Run the unit tests in the shell", &loaded)
            .top
            .iter()
            .any(|s| s.toolset == "shell")
    );
}

#[test]
fn usage_reorders_relevant_toolsets_but_never_recommends_alone() {
    let eval = eval();
    let usage = Arc::new(InMemoryUsage::default());
    let plain = Recommender::new(docs(&eval)).rank("search for it");
    assert_eq!(plain[0].toolset, "files");
    usage.record_ok("ws1", "web");
    usage.record_ok("ws1", "web");
    let with_usage = Recommender::new(docs(&eval)).with_usage(usage.clone(), "ws1");
    assert_eq!(with_usage.rank("search for it")[0].toolset, "web");
    let other_scope = Recommender::new(docs(&eval)).with_usage(usage, "ws2");
    assert_eq!(other_scope.rank("search for it")[0].toolset, "files");
    assert!(
        with_usage.recommend("zzz qqq", &[]).top.is_empty(),
        "usage alone never recommends"
    );
}

#[test]
fn record_event_counts_only_ok_calls_of_real_toolsets() {
    let record = |toolset: Option<&str>, class: &str| -> AiRunToolRecordEventData {
        let mut value = json!({
            "toolCallId": "c1", "toolName": "shell_run", "step": 1, "model": "m",
            "outcomeClass": class, "aliasesUsed": [], "valuesConverted": 0,
            "salvaged": false, "repairAttempt": 0, "durationMs": 1,
            "argumentBytes": 2, "resultBytes": 3
        });
        if let Some(toolset) = toolset {
            value["toolset"] = json!(toolset);
        }
        serde_json::from_value(value).expect("record")
    };
    let store = InMemoryUsage::default();
    assert!(record_usage(&store, "s", &record(Some("shell"), "ok")));
    assert!(record_usage(
        &store,
        "s",
        &record(Some("shell"), "ok_after_repair")
    ));
    assert!(!record_usage(
        &store,
        "s",
        &record(Some("shell"), "tool_error")
    ));
    assert!(!record_usage(&store, "s", &record(Some("meta"), "ok")));
    assert!(!record_usage(&store, "s", &record(None, "ok")));
    assert_eq!(store.counts("s"), BTreeMap::from([("shell".to_owned(), 2)]));
}

#[test]
fn promotion_is_off_by_default_and_requests_the_top_toolset_when_on() {
    let eval = eval();
    let request = "Run the unit tests in the shell";
    assert_eq!(
        Recommender::new(docs(&eval))
            .recommend(request, &[])
            .promote,
        None
    );
    let on = Recommender::new(docs(&eval)).with_options(RecommenderOptions {
        promote_top: true,
        ..RecommenderOptions::default()
    });
    let advice = on.recommend(request, &[]);
    assert_eq!(advice.promote.as_deref(), Some("shell"));
    let promotions = Promotions::default();
    assert!(advice.request_promotion(&promotions));
    assert!(!advice.request_promotion(&promotions), "already pending");
}

struct Shouting;
impl ToolsetScorer for Shouting {
    fn score(&self, _request: &str, docs: &[ToolsetDoc]) -> Vec<f64> {
        docs.iter()
            .map(|d| if d.id == "web" { 1.0 } else { 0.0 })
            .collect()
    }
}

#[test]
fn a_vector_style_scorer_plugs_into_the_seam() {
    let recommender = Recommender::new(docs(&eval())).with_scorer(Arc::new(Shouting), 5.0);
    assert_eq!(recommender.rank("zzz")[0].toolset, "web");
}

fn definition(name: &str, capability: &str) -> ToolDefinition {
    serde_json::from_value(json!({
        "name": name, "version": "1", "effect": "read", "capability": capability,
        "description": "d", "inputSchema": {"type": "object", "properties": {}}
    }))
    .unwrap()
}

#[test]
fn no_toolset_is_removed_from_catalog_or_ranking() {
    let eval = eval();
    let toolsets: BTreeMap<String, Toolset> = eval
        .toolsets
        .iter()
        .map(|s| (s.id.clone(), Toolset::new(&s.id, &s.title, &s.card)))
        .chain([(META_TOOLSET.to_owned(), meta_toolset())])
        .collect();
    let defs: Vec<ToolDefinition> = eval
        .toolsets
        .iter()
        .flat_map(|s| {
            s.tools
                .iter()
                .map(|t| definition(&t.name, &format!("{}.read", s.id)))
        })
        .collect();
    let names: BTreeMap<String, String> = eval
        .toolsets
        .iter()
        .flat_map(|s| s.tools.iter().map(|t| (t.name.clone(), s.id.clone())))
        .collect();
    let view = ToolsetView::new(toolsets, names);
    let before: Vec<String> = view.toolsets().map(|t| t.id.clone()).collect();
    let from_view = docs_from_view(&view, &defs);
    assert_eq!(
        from_view.len(),
        eval.toolsets.len(),
        "meta is not a recommendation target"
    );
    let recommender = Recommender::new(from_view);
    for request in &eval.requests {
        let ranked = recommender.rank(&request.text);
        assert_eq!(
            ranked.len(),
            eval.toolsets.len(),
            "ranking covers every toolset"
        );
        let _ = recommender.recommend(&request.text, &[]);
    }
    let after: Vec<String> = view.toolsets().map(|t| t.id.clone()).collect();
    assert_eq!(before, after);
    assert_eq!(defs.len(), 17);
}
