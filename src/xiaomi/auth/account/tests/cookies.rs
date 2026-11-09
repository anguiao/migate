use super::super::cookies::remove_expired_cookies;
use isahc::{
    cookies::{Cookie, CookieJar},
    http::{HeaderMap, HeaderValue, Uri, header::SET_COOKIE},
};
use std::time::{Duration, SystemTime};

fn uri() -> Uri {
    "https://account.xiaomi.com/oauth2/authorize"
        .parse()
        .unwrap()
}

fn add(jar: &CookieJar, name: &str, value: &str, domain: &str, uri: &Uri) {
    jar.set(
        Cookie::builder(name, value)
            .domain(domain)
            .path("/")
            .build()
            .unwrap(),
        uri,
    )
    .unwrap();
}

fn values(jar: &CookieJar, uri: &Uri, name: &str) -> Vec<String> {
    jar.get_for_uri(uri)
        .into_iter()
        .filter(|cookie| cookie.name() == name)
        .map(|cookie| cookie.value().to_owned())
        .collect()
}

#[test]
fn removes_expired_hyphenated_cookies_only_in_their_scope() {
    let jar = CookieJar::new();
    let uri = uri();
    add(&jar, "cUserId", "", "xiaomi.com", &uri);
    add(&jar, "cUserId", "current", "account.xiaomi.com", &uri);
    let mut headers = HeaderMap::new();
    headers.append(
        SET_COOKIE,
        HeaderValue::from_static(
            "cUserId=; Domain=.xiaomi.com; Path=/; Expires=Thu, 01-Jan-1970 00:00:00 GMT",
        ),
    );

    remove_expired_cookies(
        &jar,
        &uri,
        &headers,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1),
    );

    assert_eq!(values(&jar, &uri, "cUserId"), ["current"]);
}

#[test]
fn later_cookie_for_the_same_scope_wins_over_an_expired_cookie() {
    let jar = CookieJar::new();
    let uri = uri();
    add(&jar, "userId", "current", "account.xiaomi.com", &uri);
    let mut headers = HeaderMap::new();
    headers.append(
        SET_COOKIE,
        HeaderValue::from_static(
            "userId=; Domain=account.xiaomi.com; Path=/; Expires=Thu, 01-Jan-1970 00:00:00 GMT",
        ),
    );
    headers.append(
        SET_COOKIE,
        HeaderValue::from_static("userId=current; Domain=account.xiaomi.com; Path=/"),
    );

    remove_expired_cookies(
        &jar,
        &uri,
        &headers,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1),
    );

    assert_eq!(values(&jar, &uri, "userId"), ["current"]);
}

#[test]
fn keeps_unexpired_cookies_and_honors_max_age() {
    let jar = CookieJar::new();
    let uri = uri();
    add(&jar, "first", "", "account.xiaomi.com", &uri);
    add(&jar, "second", "", "account.xiaomi.com", &uri);
    let mut headers = HeaderMap::new();
    headers.append(
        SET_COOKIE,
        HeaderValue::from_static(
            "first=; Domain=account.xiaomi.com; Path=/; Expires=Fri, 02-Jan-1970 00:00:00 GMT",
        ),
    );
    headers.append(SET_COOKIE, HeaderValue::from_static("second=; Domain=account.xiaomi.com; Path=/; Expires=Thu, 01-Jan-1970 00:00:00 GMT; Max-Age=3600"));

    remove_expired_cookies(&jar, &uri, &headers, SystemTime::UNIX_EPOCH);

    assert_eq!(values(&jar, &uri, "first"), [""]);
    assert_eq!(values(&jar, &uri, "second"), [""]);
}
