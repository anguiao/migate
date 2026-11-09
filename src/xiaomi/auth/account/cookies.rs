use isahc::{
    cookies::{Cookie, CookieJar},
    http::{HeaderMap, Uri, header::SET_COOKIE},
};
use std::{collections::HashMap, time::SystemTime};

#[derive(Eq, Hash, PartialEq)]
struct Scope {
    name: String,
    domain: String,
    path: String,
}

/// Remove expired empty cookies whose hyphenated dates the HTTP client cannot parse.
pub(super) fn remove_expired_cookies(
    jar: &CookieJar,
    uri: &Uri,
    headers: &HeaderMap,
    now: SystemTime,
) {
    let Some(host) = uri.host() else { return };
    let mut last = HashMap::new();
    for header in headers.get_all(SET_COOKIE) {
        let Ok(header) = header.to_str() else {
            continue;
        };
        let mut parts = header.trim_start().split(';');
        let Some((name, value)) = parts.next().and_then(|part| part.split_once('=')) else {
            continue;
        };
        let unquoted = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .unwrap_or(value);
        if Cookie::builder(name, unquoted).build().is_err() {
            continue;
        }
        let mut domain = host.to_ascii_lowercase();
        let mut path = default_path(uri.path()).to_owned();
        let mut expires = None;
        let mut max_age = false;
        for part in parts {
            let Some((key, value)) = part.trim_start().split_once('=') else {
                continue;
            };
            if key.eq_ignore_ascii_case("domain") {
                domain = value.trim_start_matches('.').to_ascii_lowercase();
            } else if key.eq_ignore_ascii_case("path") {
                path = value.to_owned();
            } else if key.eq_ignore_ascii_case("expires") && expires.is_none() {
                expires = Some(value);
            } else if key.eq_ignore_ascii_case("max-age") {
                max_age |= value.parse::<u64>().is_ok();
            }
        }
        if !domain.contains('.')
            || !(host.eq_ignore_ascii_case(&domain)
                || host
                    .to_ascii_lowercase()
                    .strip_suffix(&domain)
                    .is_some_and(|prefix| prefix.ends_with('.')))
        {
            continue;
        }
        let expired = unquoted.is_empty()
            && !max_age
            && expires.is_some_and(|value| {
                httpdate::parse_http_date(value).is_err()
                    && httpdate::parse_http_date(&value.replace('-', " "))
                        .is_ok_and(|time| time <= now)
            });
        last.insert(
            Scope {
                name: name.to_owned(),
                domain,
                path,
            },
            expired,
        );
    }

    for (scope, expired) in last {
        if expired
            && let Ok(cookie) = Cookie::builder(scope.name, "")
                .domain(scope.domain)
                .path(scope.path)
                .expiration(SystemTime::UNIX_EPOCH)
                .build()
        {
            let _ = jar.set(cookie, uri);
        }
    }
}

fn default_path(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(index) => &path[..index],
    }
}
