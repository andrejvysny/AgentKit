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
    if value.contains('@') || value.contains('/') || value.trim() != value {
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
