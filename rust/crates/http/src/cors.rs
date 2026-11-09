//! Response decoration for cross-origin browser callers.
//!
//! Behavior is pinned to the TypeScript reference transport
//! (`packages/transport-http/src/cors.ts`) so a mounted native server and
//! the retired TypeScript handler answer identically:
//!  - Unconfigured (the default) — no response carries a CORS header and
//!    `OPTIONS` answers 405 exactly as before.
//!  - A configured deployment decorates EVERY response: a matched `Origin`
//!    earns `access-control-allow-origin` (the literal `*` for a wildcard
//!    configuration, the exact origin string for a listed one) plus
//!    `access-control-expose-headers` when configured; an unmatched or
//!    absent `Origin` still earns `Vary: Origin` so caches never collapse
//!    the two variants. A non-matching origin is NOT an error — answering
//!    403 here would break every non-browser client.
//!  - A preflight (OPTIONS carrying a matched `Origin`) answers 204 before
//!    body limits and before authentication: `access-control-allow-methods`
//!    comes from the route table's methods for that exact path,
//!    `access-control-allow-headers` from the configuration or the
//!    transport defaults, and `access-control-max-age` only when
//!    configured.
//!  - `Access-Control-Allow-Credentials` is deliberately never emitted.

use axum::http::{HeaderMap, HeaderValue};

/// The TypeScript transport's frozen preflight allow-list
/// (`DEFAULT_ALLOW_HEADERS`, `packages/transport-http/src/cors.ts`).
pub const DEFAULT_ALLOW_HEADERS: [&str; 4] = [
    "Content-Type",
    "Idempotency-Key",
    "Last-Event-ID",
    "Authorization",
];

/// Which `Origin` values are served. `Any` echoes the literal `*`; a list
/// matches by exact string comparison and echoes the matched string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorsOrigins {
    Any,
    List(Vec<String>),
}

/// One configured CORS surface; absent means no header is ever added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorsConfig {
    pub origins: CorsOrigins,
    /// Preflight `access-control-allow-headers`; absent uses the defaults.
    pub allow_headers: Option<Vec<String>>,
    /// `access-control-expose-headers`; absent omits the header entirely.
    pub expose_headers: Option<Vec<String>>,
    /// Preflight `access-control-max-age`; absent omits the header.
    pub max_age_seconds: Option<u32>,
}

impl CorsConfig {
    /// The matched origin's header value — `Some("*")` for the wildcard,
    /// `Some(origin)` for a listed origin, `None` when no `Origin` header
    /// is present or no configured origin matches.
    pub fn matched_origin(&self, headers: &HeaderMap) -> Option<String> {
        let origin = headers
            .get("origin")
            .and_then(|value| value.to_str().ok())?
            .to_owned();
        match &self.origins {
            CorsOrigins::Any => Some("*".to_owned()),
            CorsOrigins::List(list) => list.contains(&origin).then_some(origin),
        }
    }

    fn joined(values: &[String]) -> HeaderValue {
        // Values are caller-supplied strings; a value carrying forbidden
        // characters degrades to omitting THAT header rather than
        // poisoning the whole response.
        HeaderValue::from_str(&values.join(", ")).unwrap_or_else(|_| HeaderValue::from_static(""))
    }

    fn expose_into(&self, headers: &mut HeaderMap) {
        if let Some(expose) = &self.expose_headers {
            let value = Self::joined(expose);
            if !value.is_empty() {
                headers.insert("access-control-expose-headers", value);
            }
        }
    }

    /// Decorate an ordinary (non-preflight) response. `matched` is the
    /// `matched_origin` result for THIS request; the caller passes it for
    /// every response so 404/405/413/401/500 and SSE streams carry the
    /// same decoration as route successes.
    pub fn decorate(&self, headers: &mut HeaderMap, matched: Option<&str>) {
        headers.insert("vary", HeaderValue::from_static("Origin"));
        if let Some(origin) = matched {
            if let Ok(value) = HeaderValue::from_str(origin) {
                headers.insert("access-control-allow-origin", value);
            }
            self.expose_into(headers);
        }
    }

    /// Build the 204 preflight answer for a matched origin. `methods` are
    /// the route table's methods for the requested path, deduplicated in
    /// route-table order.
    pub fn preflight(&self, matched: &str, methods: &[&str]) -> axum::response::Response {
        let mut headers = HeaderMap::new();
        if let Ok(value) = HeaderValue::from_str(matched) {
            headers.insert("access-control-allow-origin", value);
        }
        headers.insert("vary", HeaderValue::from_static("Origin"));
        self.expose_into(&mut headers);
        headers.insert(
            "access-control-allow-methods",
            Self::joined(
                &methods
                    .iter()
                    .map(|method| (*method).to_owned())
                    .collect::<Vec<_>>(),
            ),
        );
        let allow = self.allow_headers.clone().unwrap_or_else(|| {
            DEFAULT_ALLOW_HEADERS
                .into_iter()
                .map(str::to_owned)
                .collect()
        });
        let value = Self::joined(&allow);
        if !value.is_empty() {
            headers.insert("access-control-allow-headers", value);
        }
        if let Some(max_age) = self.max_age_seconds {
            headers.insert("access-control-max-age", HeaderValue::from(max_age));
        }
        let mut response = axum::response::Response::new(axum::body::Body::empty());
        *response.status_mut() = axum::http::StatusCode::NO_CONTENT;
        *response.headers_mut() = headers;
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with(origin: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "origin",
            HeaderValue::from_str(origin).expect("test origin"),
        );
        headers
    }

    #[test]
    fn list_matches_exactly_and_echoes() {
        let cors = CorsConfig {
            origins: CorsOrigins::List(vec!["http://localhost:5173".to_owned()]),
            allow_headers: None,
            expose_headers: None,
            max_age_seconds: None,
        };
        assert_eq!(
            cors.matched_origin(&headers_with("http://localhost:5173"))
                .as_deref(),
            Some("http://localhost:5173")
        );
        assert_eq!(
            cors.matched_origin(&headers_with("http://evil.example")),
            None
        );
        assert_eq!(cors.matched_origin(&HeaderMap::new()), None);
    }

    #[test]
    fn wildcard_echoes_literal_star_for_any_origin() {
        let cors = CorsConfig {
            origins: CorsOrigins::Any,
            allow_headers: None,
            expose_headers: None,
            max_age_seconds: None,
        };
        assert_eq!(
            cors.matched_origin(&headers_with("http://anywhere"))
                .as_deref(),
            Some("*")
        );
        // No Origin header at all — not a CORS request, no decoration value.
        assert_eq!(cors.matched_origin(&HeaderMap::new()), None);
    }

    #[test]
    fn decoration_always_carries_vary_and_adds_allow_origin_only_when_matched() {
        let cors = CorsConfig {
            origins: CorsOrigins::List(vec!["http://a".to_owned()]),
            allow_headers: None,
            expose_headers: Some(vec!["X-Total-Count".to_owned()]),
            max_age_seconds: None,
        };
        let mut matched = HeaderMap::new();
        cors.decorate(&mut matched, Some("http://a"));
        assert_eq!(matched["access-control-allow-origin"], "http://a");
        assert_eq!(matched["vary"], "Origin");
        assert_eq!(matched["access-control-expose-headers"], "X-Total-Count");
        let mut unmatched = HeaderMap::new();
        cors.decorate(&mut unmatched, None);
        assert_eq!(unmatched["vary"], "Origin");
        assert!(!unmatched.contains_key("access-control-allow-origin"));
        // Never credentials.
        assert!(!matched.contains_key("access-control-allow-credentials"));
    }

    #[test]
    fn preflight_answers_204_with_route_methods_and_defaults() {
        let cors = CorsConfig {
            origins: CorsOrigins::List(vec!["http://a".to_owned()]),
            allow_headers: None,
            expose_headers: None,
            max_age_seconds: Some(600),
        };
        let response = cors.preflight("http://a", &["GET", "POST"]);
        assert_eq!(response.status(), axum::http::StatusCode::NO_CONTENT);
        let headers = response.headers();
        assert_eq!(headers["access-control-allow-origin"], "http://a");
        assert_eq!(headers["access-control-allow-methods"], "GET, POST");
        assert_eq!(
            headers["access-control-allow-headers"],
            "Content-Type, Idempotency-Key, Last-Event-ID, Authorization"
        );
        assert_eq!(headers["access-control-max-age"], "600");
        assert_eq!(headers["vary"], "Origin");
        assert!(!headers.contains_key("access-control-expose-headers"));
    }

    #[test]
    fn preflight_custom_allow_headers_and_absent_max_age() {
        let cors = CorsConfig {
            origins: CorsOrigins::Any,
            allow_headers: Some(vec!["Content-Type".to_owned(), "X-Tenant".to_owned()]),
            expose_headers: None,
            max_age_seconds: None,
        };
        let response = cors.preflight("*", &["GET"]);
        let headers = response.headers();
        assert_eq!(
            headers["access-control-allow-headers"],
            "Content-Type, X-Tenant"
        );
        assert!(!headers.contains_key("access-control-max-age"));
    }
}
