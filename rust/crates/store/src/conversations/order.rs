use super::MessageRecord;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};

fn prefix(record: &MessageRecord) -> bool {
    record.role == "tool" || internal_assistant(record)
}

fn internal_assistant(record: &MessageRecord) -> bool {
    record.role == "assistant" && record.metadata.get("internal") == Some(&Value::Bool(true))
}

fn order_run(records: Vec<MessageRecord>) -> Vec<MessageRecord> {
    if records
        .iter()
        .any(|m| m.metadata.get("canonicalProviderTurn") == Some(&Value::Bool(true)))
    {
        return records;
    }
    let mut tools = HashMap::<String, Vec<MessageRecord>>::new();
    for record in records.iter().filter(|m| m.role == "tool") {
        if let Some(call) = &record.tool_call_id {
            tools.entry(call.clone()).or_default().push(record.clone());
        }
    }
    let mut claimed = HashSet::new();
    let mut groups = HashMap::new();
    for record in records.iter().filter(|m| internal_assistant(m)) {
        let mut results = Vec::new();
        for call in record
            .tool_calls
            .as_ref()
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(id) = call.get("id").and_then(Value::as_str)
                && let Some(group) = tools.remove(id)
            {
                for result in &group {
                    claimed.insert(result.id.clone());
                }
                results.extend(group);
            }
        }
        groups.insert(record.id.clone(), results);
    }
    let mut ordered = Vec::new();
    for record in records
        .iter()
        .filter(|m| prefix(m) && !claimed.contains(&m.id))
    {
        ordered.push(record.clone());
        if let Some(group) = groups.remove(&record.id) {
            ordered.extend(group);
        }
    }
    ordered.extend(records.into_iter().filter(|m| !prefix(m)));
    ordered
}

/// Reorders only each run's existing slots, preserving conversation order between runs.
pub fn order_messages_for_provider(mut records: Vec<MessageRecord>) -> Vec<MessageRecord> {
    records.sort_by_key(|record| record.order_key);
    let mut slots = BTreeMap::<String, Vec<usize>>::new();
    for (position, record) in records.iter().enumerate() {
        if record.role == "user" {
            continue;
        }
        if let Some(run) = &record.run_id {
            slots.entry(run.clone()).or_default().push(position);
        }
    }
    let mut ordered = records.clone();
    for positions in slots.values() {
        let run = order_run(
            positions
                .iter()
                .map(|index| records[*index].clone())
                .collect(),
        );
        for (position, record) in positions.iter().zip(run) {
            ordered[*position] = record;
        }
    }
    ordered
}
