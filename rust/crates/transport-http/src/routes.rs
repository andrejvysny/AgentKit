use serde::Deserialize;
use std::{collections::BTreeMap, sync::OnceLock};

#[derive(Debug, Deserialize)]
pub struct Route {
    pub operation: String,
    pub method: String,
    pub path: String,
}

pub fn routes() -> &'static [Route] {
    static ROUTES: OnceLock<Vec<Route>> = OnceLock::new();
    ROUTES.get_or_init(|| {
        agentkit_contracts::REST_ROUTES
            .iter()
            .map(|route| Route {
                operation: route.operation.as_str().into(),
                method: route.method.as_str().into(),
                path: route.path.into(),
            })
            .collect()
    })
}

pub(crate) fn match_path(route: &Route, path: &str) -> Option<BTreeMap<String, String>> {
    let pattern: Vec<_> = route.path.split('/').filter(|s| !s.is_empty()).collect();
    let actual: Vec<_> = path.split('/').filter(|s| !s.is_empty()).collect();
    if pattern.len() != actual.len() {
        return None;
    }
    let mut params = BTreeMap::new();
    for (expected, got) in pattern.into_iter().zip(actual) {
        if let Some(name) = expected.strip_prefix(':') {
            let decoded = percent_encoding::percent_decode_str(got)
                .decode_utf8()
                .map(|s| s.into_owned())
                .unwrap_or_else(|_| got.into());
            params.insert(name.to_owned(), decoded);
        } else if expected != got {
            return None;
        }
    }
    Some(params)
}
