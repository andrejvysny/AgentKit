use crate::ServerOptions;
use http::{HeaderMap, uri::Authority};
use url::Url;

pub(crate) fn allowed(headers: &HeaderMap, options: &ServerOptions) -> bool {
    let Some(host) = headers
        .get("host")
        .and_then(|host| host.to_str().ok())
        .and_then(authority)
    else {
        return false;
    };
    if headers.get_all("host").iter().count() != 1 {
        return false;
    }
    let host_allowed = options
        .allowed_hosts
        .iter()
        .filter_map(|entry| authority(entry))
        .any(|entry| {
            entry.host().eq_ignore_ascii_case(host.host())
                && (entry.port().is_none() || entry.port_u16() == host.port_u16())
        });
    if !host_allowed {
        return false;
    }
    let Some(origins) = &options.allowed_origins else {
        return true;
    };
    let Some(origin) = headers.get("origin") else {
        return true;
    };
    if headers.get_all("origin").iter().count() != 1 {
        return false;
    }
    let Some(actual) = origin.to_str().ok().and_then(normalize_origin) else {
        return false;
    };
    origins
        .iter()
        .filter_map(|entry| normalize_origin(entry))
        .any(|entry| entry == actual)
}

fn authority(value: &str) -> Option<Authority> {
    // A trailing colon claims a port without naming one, and a leading one a
    // port behind no host; the http parser accepts both shapes, the guard
    // must not.
    if value.contains('@')
        || value.contains('/')
        || value.trim() != value
        || value.ends_with(':')
        || value.starts_with(':')
    {
        return None;
    }
    value.parse().ok()
}

fn normalize_origin(value: &str) -> Option<String> {
    let url = Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return None;
    }
    Some(url.origin().ascii_serialization().to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use std::time::Duration;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(
                *name,
                HeaderValue::from_str(value).expect("test header value"),
            );
        }
        headers
    }

    fn options(allowed_hosts: &[&str], allowed_origins: Option<Vec<String>>) -> ServerOptions {
        ServerOptions {
            writes_enabled: false,
            allowed_hosts: allowed_hosts
                .iter()
                .map(|entry| (*entry).to_owned())
                .collect(),
            allowed_origins,
            max_sessions_per_principal: 64,
            session_idle_ttl: Duration::from_secs(1800),
            max_request_bytes: 4 * 1024 * 1024,
            max_concurrent_calls: 4,
            max_batch_size: 8,
            max_call_duration: Duration::from_secs(120),
            tool_page_size: 200,
            call_admissions: None,
        }
    }

    fn loopback(origins: Option<Vec<String>>) -> ServerOptions {
        options(&["localhost", "127.0.0.1", "[::1]"], origins)
    }

    // — splitHostPort equivalents —

    #[test]
    fn authority_splits_hostname_and_port_ignoring_case() {
        let split = authority("LocalHost:8787").expect("hostname and port parse");
        assert_eq!(split.host(), "LocalHost");
        assert_eq!(split.port_u16(), Some(8787));
        let split = authority("127.0.0.1").expect("bare IPv4 parses");
        assert_eq!(split.host(), "127.0.0.1");
        assert_eq!(split.port_u16(), None);
        // The comparison itself is case-insensitive: an uppercase Host matches
        // a lowercase allow-list entry, exactly as the TypeScript split does
        // after lowercasing.
        assert!(allowed(
            &headers(&[("host", "LocalHost:8787")]),
            &options(&["localhost"], None)
        ));
    }

    #[test]
    fn authority_keeps_ipv6_brackets() {
        let split = authority("[::1]:9000").expect("bracketed IPv6 with port parses");
        assert_eq!(split.host(), "[::1]");
        assert_eq!(split.port_u16(), Some(9000));
        let split = authority("[::1]").expect("bracketed IPv6 without port parses");
        assert_eq!(split.host(), "[::1]");
        assert_eq!(split.port_u16(), None);
    }

    #[test]
    fn authority_refuses_what_it_cannot_honestly_parse() {
        for shape in ["", "[::1", "::1", "host:", ":8080"] {
            assert!(authority(shape).is_none(), "{shape:?} must not parse");
        }
        // Guard-specific refusals: credentials, paths, and padding never
        // smuggle an entry past the allow list.
        for shape in ["user@evil.test", "evil.test/", " evil.test", "evil.test "] {
            assert!(authority(shape).is_none(), "{shape:?} must not parse");
        }
    }

    // — isHostAllowed equivalents —

    #[test]
    fn entry_without_port_matches_any_port() {
        let options = options(&["localhost"], None);
        assert!(allowed(&headers(&[("host", "localhost:8787")]), &options));
        assert!(allowed(&headers(&[("host", "localhost")]), &options));
    }

    #[test]
    fn entry_with_port_requires_the_exact_port() {
        let options = options(&["localhost:8787"], None);
        assert!(allowed(&headers(&[("host", "localhost:8787")]), &options));
        assert!(!allowed(&headers(&[("host", "localhost:9999")]), &options));
    }

    #[test]
    fn host_outside_the_list_or_missing_header_is_refused() {
        let options = loopback(None);
        assert!(!allowed(&headers(&[("host", "evil.com")]), &options));
        assert!(!allowed(&headers(&[("host", "evil.com:8787")]), &options));
        assert!(!allowed(&headers(&[]), &options));
        // A Host header that appears twice is not one host anyone vouched for.
        assert!(!allowed(
            &headers(&[("host", "localhost"), ("host", "127.0.0.1")]),
            &options
        ));
    }

    #[test]
    fn default_loopback_entries_cover_any_port() {
        let options = loopback(None);
        for host in ["localhost:1", "127.0.0.1:65535", "[::1]:8787"] {
            assert!(
                allowed(&headers(&[("host", host)]), &options),
                "{host} must be allowed on any port"
            );
        }
    }

    // — isOriginAllowed equivalents —

    #[test]
    fn origin_comparison_is_parsed_so_a_trailing_slash_is_not_a_lockout() {
        let options = loopback(Some(vec!["http://localhost:5173/".into()]));
        assert!(allowed(
            &headers(&[("host", "localhost"), ("origin", "http://localhost:5173")]),
            &options
        ));
    }

    #[test]
    fn origin_scheme_host_or_port_mismatch_is_refused() {
        let options = loopback(Some(vec!["http://localhost:5173".into()]));
        for origin in [
            "https://localhost:5173",
            "http://localhost:5174",
            "http://evil.com",
        ] {
            assert!(
                !allowed(
                    &headers(&[("host", "localhost"), ("origin", origin)]),
                    &options
                ),
                "{origin} must be refused"
            );
        }
    }

    #[test]
    fn origin_opaque_or_unparseable_values_are_refused() {
        let options = loopback(Some(vec!["http://localhost".into()]));
        for origin in ["null", "not a url", "file:///etc/passwd", ""] {
            assert!(
                !allowed(
                    &headers(&[("host", "localhost"), ("origin", origin)]),
                    &options
                ),
                "{origin:?} must be refused"
            );
        }
        assert!(
            !allowed(
                &headers(&[
                    ("host", "localhost"),
                    ("origin", "http://localhost"),
                    ("origin", "http://evil.com"),
                ]),
                &options
            ),
            "two Origin headers is not one origin anyone vouched for"
        );
    }

    #[test]
    fn origin_wildcard_entry_is_never_honored() {
        let options = loopback(Some(vec!["*".into()]));
        assert!(!allowed(
            &headers(&[("host", "localhost"), ("origin", "http://evil.com")]),
            &options
        ));
    }

    #[test]
    fn origin_is_ignored_entirely_when_unconfigured() {
        let options = loopback(None);
        assert!(allowed(
            &headers(&[("host", "localhost"), ("origin", "http://evil.com")]),
            &options
        ));
    }

    #[test]
    fn origin_is_checked_only_when_present_once_configured() {
        let options = loopback(Some(vec!["http://ok.test".into()]));
        assert!(allowed(&headers(&[("host", "localhost")]), &options));
        assert!(allowed(
            &headers(&[("host", "localhost"), ("origin", "http://ok.test")]),
            &options
        ));
        assert!(!allowed(
            &headers(&[("host", "localhost"), ("origin", "http://evil.com")]),
            &options
        ));
    }
}
