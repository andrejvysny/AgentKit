//! CE-07: cache capability, strategy and the explicit-breakpoint planner.

use agentkit_core::*;
use serde_json::json;

fn layout() -> PromptLayout {
    PromptLayout::of(Some("policy"), Some("rules"), Some("memory"))
}
fn tool(name: &str) -> ToolDefinition {
    serde_json::from_value(json!({"name":name,"version":"1","effect":"read","capability":"c","description":"d","inputSchema":{"type":"object"}})).unwrap()
}
fn prefix(tools: &[ToolDefinition]) -> agentkit_contracts::AiSessionPrefix {
    let digests = PrefixDigests::compute(&layout(), tools, &json!({"model":"m"}));
    freeze(None, digests, "chat", "branch", "task").prefix().clone()
}
fn anthropic() -> CacheCapability {
    CacheCapability::ExplicitBreakpoints {
        max_breakpoints: 4,
        ttls: vec![CacheTtl::FiveMinutes, CacheTtl::OneHour],
        min_tokens: 1024,
    }
}
fn shape() -> BreakpointShape {
    BreakpointShape {
        has_tools: true,
        last_system: Some(Segment::Memory),
        prefix_tokens: 5000,
        previous_end: Some(6),
        request_end: Some(8),
    }
}
fn at(point: &Breakpoint) -> (BreakpointAt, CacheTtl) {
    (point.at, point.ttl)
}

#[test]
fn implicit_prefix_plans_the_session_key_with_its_field() {
    let prefix = prefix(&[]);
    let plan = CacheCapability::ImplicitPrefix {
        hint: Some(CacheHint::PromptCacheKey),
    }
    .plan(&layout(), &prefix);
    assert_eq!(plan.cache_key.as_deref(), Some(prefix.cache_key.as_str()));
    assert_eq!(plan.key_field, Some(CacheHint::PromptCacheKey));
    assert!(plan.breakpoints.is_empty());
}

#[test]
fn none_and_hintless_implicit_plan_nothing() {
    let prefix = prefix(&[]);
    assert!(CacheCapability::None.plan(&layout(), &prefix).is_empty());
    let plain = CacheCapability::ImplicitPrefix { hint: None };
    assert!(plain.plan(&layout(), &prefix).is_empty());
}

#[test]
fn local_slot_plans_pinning_without_a_key() {
    let prefix = prefix(&[]);
    let plan = CacheCapability::LocalSlot { pinning: true }.plan(&layout(), &prefix);
    assert!(plan.pin_slot && plan.cache_key.is_none());
}

#[test]
fn explicit_planner_places_four_breakpoints_in_request_order() {
    let points = plan_breakpoints(&anthropic(), &shape(), CacheTtl::OneHour);
    let got: Vec<_> = points.iter().map(at).collect();
    assert_eq!(
        got,
        [
            (BreakpointAt::LastTool, CacheTtl::OneHour),
            (BreakpointAt::EndOfSegment(Segment::Memory), CacheTtl::OneHour),
            (BreakpointAt::AfterMessage(6), CacheTtl::FiveMinutes),
            (BreakpointAt::AfterMessage(8), CacheTtl::FiveMinutes),
        ]
    );
}

#[test]
fn long_ttl_is_per_profile_and_falls_back_when_unsupported() {
    let short = plan_breakpoints(&anthropic(), &shape(), CacheTtl::FiveMinutes);
    assert!(short.iter().all(|point| point.ttl == CacheTtl::FiveMinutes));
    let five_only = CacheCapability::ExplicitBreakpoints {
        max_breakpoints: 4,
        ttls: vec![CacheTtl::FiveMinutes],
        min_tokens: 0,
    };
    let points = plan_breakpoints(&five_only, &shape(), CacheTtl::OneHour);
    assert!(points.iter().all(|point| point.ttl == CacheTtl::FiveMinutes));
}

#[test]
fn small_prefix_gets_no_prefix_breakpoints() {
    let small = BreakpointShape {
        prefix_tokens: 100,
        ..shape()
    };
    let got: Vec<_> = plan_breakpoints(&anthropic(), &small, CacheTtl::OneHour)
        .iter()
        .map(at)
        .collect();
    assert_eq!(
        got,
        [
            (BreakpointAt::AfterMessage(6), CacheTtl::FiveMinutes),
            (BreakpointAt::AfterMessage(8), CacheTtl::FiveMinutes),
        ]
    );
}

#[test]
fn over_budget_drops_bp3_then_bp1() {
    let capability = |max| CacheCapability::ExplicitBreakpoints {
        max_breakpoints: max,
        ttls: vec![CacheTtl::OneHour],
        min_tokens: 0,
    };
    let three = plan_breakpoints(&capability(3), &shape(), CacheTtl::OneHour);
    assert_eq!(three.len(), 3);
    assert!(three.iter().all(|point| point.at != BreakpointAt::AfterMessage(6)));
    let two = plan_breakpoints(&capability(2), &shape(), CacheTtl::OneHour);
    let got: Vec<_> = two.iter().map(|point| point.at).collect();
    assert_eq!(
        got,
        [
            BreakpointAt::EndOfSegment(Segment::Memory),
            BreakpointAt::AfterMessage(8)
        ]
    );
}

#[test]
fn first_request_has_no_bp3_and_no_tools_means_no_bp1() {
    let first = BreakpointShape {
        has_tools: false,
        previous_end: None,
        ..shape()
    };
    let points = plan_breakpoints(&anthropic(), &first, CacheTtl::OneHour);
    assert_eq!(points.len(), 2);
    assert_eq!(points[0].at, BreakpointAt::EndOfSegment(Segment::Memory));
}

#[test]
fn strategy_plan_derives_the_static_sites_from_layout_and_prefix() {
    let anthropic = CacheCapability::ExplicitBreakpoints {
        max_breakpoints: 4,
        ttls: vec![CacheTtl::OneHour],
        min_tokens: 0,
    };
    let with_tools = anthropic.plan(&layout(), &prefix(&[tool("a")]));
    let got: Vec<_> = with_tools.breakpoints.iter().map(|point| point.at).collect();
    assert_eq!(
        got,
        [
            BreakpointAt::LastTool,
            BreakpointAt::EndOfSegment(Segment::Memory)
        ]
    );
    let no_tools = anthropic.plan(&layout(), &prefix(&[]));
    assert_eq!(no_tools.breakpoints.len(), 1);
}

#[test]
fn non_explicit_capabilities_have_no_breakpoints() {
    assert!(plan_breakpoints(&CacheCapability::None, &shape(), CacheTtl::OneHour).is_empty());
}

#[test]
fn capability_round_trips_as_tagged_json() {
    let value = serde_json::to_value(CacheCapability::ImplicitPrefix {
        hint: Some(CacheHint::SessionIdField),
    })
    .unwrap();
    assert_eq!(value, json!({"kind":"implicit_prefix","hint":"session_id_field"}));
    assert_eq!(
        serde_json::from_value::<CacheCapability>(json!({"kind":"none"})).unwrap(),
        CacheCapability::None
    );
}
