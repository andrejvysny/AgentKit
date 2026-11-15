use serde_json::Value;

#[derive(Debug, PartialEq, Eq)]
pub struct Break {
    pub position: usize,
    pub path: String,
}

fn first_difference(a: &Value, b: &Value, path: &str) -> Option<String> {
    match (a, b) {
        (Value::Object(left), Value::Object(right)) => {
            let mut l = left.iter();
            let mut r = right.iter();
            loop {
                match (l.next(), r.next()) {
                    (None, None) => break,
                    (Some((lk, lv)), Some((rk, rv))) if lk == rk => {
                        let escaped = lk.replace('~', "~0").replace('/', "~1");
                        if let Some(diff) = first_difference(lv, rv, &format!("{path}/{escaped}")) {
                            return Some(diff);
                        }
                    }
                    _ => return Some(format!("{path}/@order")),
                }
            }
            None
        }
        (Value::Array(left), Value::Array(right)) => {
            for index in 0..left.len().max(right.len()) {
                let next = format!("{path}/{index}");
                match (left.get(index), right.get(index)) {
                    (Some(x), Some(y)) => {
                        if let Some(diff) = first_difference(x, y, &next) {
                            return Some(diff);
                        }
                    }
                    _ => return Some(next),
                }
            }
            None
        }
        _ if a == b => None,
        _ => Some(path.to_owned()),
    }
}

/// Compare the actual serialized prefix through the previous last input item.
/// Offsets count UTF-8 bytes in the provider-assembled JSON value.
pub fn compare(a: &Value, b: &Value, input: &str) -> Option<Break> {
    let prefix = retained_prefix(a, input);
    let next = b.to_string();
    if next.as_bytes().starts_with(prefix.as_bytes()) {
        return None;
    }
    let position = prefix
        .bytes()
        .zip(next.bytes())
        .take_while(|(x, y)| x == y)
        .count();
    let mut retained = b.clone();
    let old_length = a[input].as_array().expect("previous input array").len();
    retained[input]
        .as_array_mut()
        .expect("next input array")
        .truncate(old_length);
    let path = first_difference(a, &retained, "").unwrap_or_else(|| "/@serialization".into());
    Some(Break { position, path })
}

fn retained_prefix(request: &Value, input: &str) -> String {
    let mut prefix = String::from("{");
    for (index, (key, value)) in request
        .as_object()
        .expect("request object")
        .iter()
        .enumerate()
    {
        if index > 0 {
            prefix.push(',');
        }
        prefix.push_str(&serde_json::to_string(key).expect("key serialization"));
        prefix.push(':');
        if key == input {
            let items = value.as_array().expect("input array");
            prefix.push('[');
            for (item_index, item) in items.iter().enumerate() {
                if item_index > 0 {
                    prefix.push(',');
                }
                prefix.push_str(&item.to_string());
            }
            return prefix;
        }
        prefix.push_str(&value.to_string());
    }
    panic!("request missing input array");
}

/// Layout check on one provider-assembled request (CE-01, INV-1): every
/// system block must sit ahead of the first non-system item of `input`, so no
/// history or turn content precedes a more stable segment. Returns the
/// JSON-pointer path of each offending item. Segment tags are not on the wire,
/// so order among the system blocks themselves is checked by the unit tests of
/// `agentkit_core::PromptLayout`.
pub fn layout_violations(request: &Value, input: &str) -> Vec<String> {
    let Some(items) = request[input].as_array() else {
        return vec![format!("/{input}")];
    };
    let mut seen_other = false;
    let mut found = vec![];
    for (index, item) in items.iter().enumerate() {
        if item["role"] == "system" {
            if seen_other {
                found.push(format!("/{input}/{index}"));
            }
        } else {
            seen_other = true;
        }
    }
    found
}

/// Number of leading system items in `input` (the system blocks on the wire).
pub fn system_block_count(request: &Value, input: &str) -> usize {
    request[input].as_array().map_or(0, |items| {
        items
            .iter()
            .take_while(|item| item["role"] == "system")
            .count()
    })
}

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize)]
pub struct EventMark {
    pub turn: usize,
    pub seq: f64,
}

pub fn exception_between(before: EventMark, after: EventMark, breaks: &[EventMark]) -> bool {
    assert!(before <= after, "request event marks must be ordered");
    breaks
        .iter()
        .any(|event| before < *event && *event <= after)
}

/// Exceptions require durable cache-break event marks captured between dispatches.
pub fn compare_recorded(
    a: &Value,
    b: &Value,
    input: &str,
    before: EventMark,
    after: EventMark,
    breaks: &[EventMark],
) -> Option<Break> {
    if exception_between(before, after, breaks) {
        None
    } else {
        compare(a, b, input)
    }
}
