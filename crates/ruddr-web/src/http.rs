//! Request parsing and response helpers shared by the routes: cookies, query
//! strings, the token checks, and the same-origin mutation check.

use crate::token::token_matches;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use serde_json::Value;

pub const COOKIE: &str = "ruddr_web";

pub fn json_response(body: &Value, status: StatusCode) -> Response {
    json_text(serde_json::to_string(body).unwrap_or_else(|_| "null".into()), status)
}

/// A JSON body that is already serialized.
pub fn json_text(body: String, status: StatusCode) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub fn failure(message: impl Into<String>, status: StatusCode) -> Response {
    json_response(&serde_json::json!({ "error": message.into() }), status)
}

pub fn text_response(body: &'static str, status: StatusCode) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain;charset=utf-8"));
    response
}

/// The cookie that carries the token for a year.
pub fn session_cookie(token: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{COOKIE}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=31536000",
        encode_component(token)
    ))
    .unwrap_or_else(|_| HeaderValue::from_static("ruddr_web=; Path=/"))
}

fn header_text(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// The value of cookie `name`, percent-decoded. A malformed encoding reads as
/// no cookie.
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let joined: Vec<&str> = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect();
    if joined.is_empty() {
        return None;
    }
    for part in joined.join("; ").split(';') {
        let mut pieces = part.trim().split('=');
        if pieces.next() == Some(name) {
            let rest: Vec<&str> = pieces.collect();
            return decode_component(&rest.join("="));
        }
    }
    None
}

/// The token from the cookie or from `Authorization`. Like the TypeScript
/// server, an `Authorization` value without the `Bearer` prefix is compared
/// whole.
pub fn is_authorized(headers: &HeaderMap, token: &str) -> bool {
    let bearer = header_text(headers, header::AUTHORIZATION).map(strip_bearer);
    token_matches(token, cookie_value(headers, COOKIE).as_deref()) || token_matches(token, bearer)
}

fn strip_bearer(value: &str) -> &str {
    if value.len() > 6 && value[..6].eq_ignore_ascii_case("bearer") {
        let rest = &value[6..];
        let trimmed = rest.trim_start();
        if trimmed.len() < rest.len() {
            return trimmed;
        }
    }
    value
}

/// Mutations need the custom header, which a cross-site form cannot send,
/// and an Origin, when present, that matches the Host the browser used. The
/// server speaks plain HTTP, so the request's own origin is `http://HOST`.
pub fn is_same_origin_mutation(headers: &HeaderMap) -> bool {
    if headers.get("x-ruddr-request").map(|value| value.as_bytes()) != Some(b"1") {
        return false;
    }
    let Some(origin) = headers.get(header::ORIGIN) else { return true };
    let Ok(origin) = origin.to_str() else { return false };
    let Some(host) = header_text(headers, header::HOST) else {
        return false;
    };
    let Some(target) = normalize_authority("http", host) else {
        return false;
    };
    // The browser's Host must already be in canonical form, as the URL parser
    // would print it.
    if target != host {
        return false;
    }
    match parse_origin(origin) {
        Some((scheme, authority)) => scheme == "http" && authority == target,
        None => false,
    }
}

/// Splits `scheme://authority` and normalizes the authority. Paths, queries,
/// and fragments are ignored; userinfo is dropped as the URL parser does.
fn parse_origin(origin: &str) -> Option<(String, String)> {
    let (scheme, rest) = origin.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let mut chars = scheme.chars();
    if !chars.next()?.is_ascii_alphabetic() || !chars.all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    Some((scheme.clone(), normalize_authority(&scheme, authority)?))
}

/// Lowercases the host and drops the scheme's default port.
fn normalize_authority(scheme: &str, authority: &str) -> Option<String> {
    if authority.is_empty() {
        return None;
    }
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (inside, after) = rest.split_once(']')?;
        let port = match after {
            "" => None,
            _ => Some(after.strip_prefix(':')?),
        };
        (format!("[{}]", inside.to_ascii_lowercase()), port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host.to_ascii_lowercase(), Some(port)),
            None => (authority.to_ascii_lowercase(), None),
        }
    };
    let bracketed = host.starts_with('[');
    let forbidden = |c: char| c.is_whitespace() || "/?#@\\".contains(c) || (!bracketed && "[]".contains(c));
    if host.is_empty() || host.contains(forbidden) {
        return None;
    }
    let default_port = match scheme {
        "http" | "ws" => Some(80),
        "https" | "wss" => Some(443),
        _ => None,
    };
    let port = match port {
        None | Some("") => None,
        Some(port) => {
            if !port.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let number: u32 = port.parse().ok().filter(|n| *n <= 65535)?;
            (Some(number) != default_port).then_some(number)
        }
    };
    Some(match port {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

/// The first value of `name` in a query string, decoded the way
/// `URLSearchParams` decodes: `+` is a space and bad escapes stay literal.
pub fn query_param(query: Option<&str>, name: &str) -> Option<String> {
    let query = query?;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        (form_decode(key) == name).then(|| form_decode(value))
    })
}

pub fn has_query_param(query: Option<&str>, name: &str) -> bool {
    query_param(query, name).is_some()
}

fn form_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() && hex(bytes[i + 1]).is_some() && hex(bytes[i + 2]).is_some() => {
                out.push(hex(bytes[i + 1]).unwrap() << 4 | hex(bytes[i + 2]).unwrap());
                i += 2;
            }
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `decodeURIComponent`: any malformed escape or invalid UTF-8 fails.
fn decode_component(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let high = hex(*bytes.get(i + 1)?)?;
            let low = hex(*bytes.get(i + 2)?)?;
            out.push(high << 4 | low);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `encodeURIComponent`.
fn encode_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn hex(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|d| d as u8)
}

/// A request body as JSON. Anything unparsable reads as `{}`, like
/// `request.json().catch(() => ({}))`.
pub fn parse_body(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap_or_else(|_| Value::Object(Default::default()))
}

/// A string field of a JSON object body. Missing fields, non-string values,
/// and non-object bodies all read as absent.
pub fn str_field<'a>(body: &'a Value, name: &str) -> Option<&'a str> {
    body.get(name).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn same_origin_requires_the_header_and_a_matching_origin() {
        let h = |origin: Option<&str>| {
            let mut pairs = vec![("host", "127.0.0.1:4519"), ("x-ruddr-request", "1")];
            if let Some(origin) = origin {
                pairs.push(("origin", origin));
            }
            is_same_origin_mutation(&headers(&pairs))
        };
        assert!(h(None));
        assert!(h(Some("http://127.0.0.1:4519")));
        assert!(!h(Some("https://evil.example")));
        assert!(!h(Some("https://127.0.0.1:4519")));
        assert!(!h(Some("null")));
        assert!(!is_same_origin_mutation(&headers(&[("host", "127.0.0.1:4519")])));
        assert!(!is_same_origin_mutation(&headers(&[
            ("host", "localhost:4519"),
            ("origin", "https://localhost:4519"),
            ("x-ruddr-request", "1")
        ])));
        assert!(is_same_origin_mutation(&headers(&[
            ("host", "[::1]:4519"),
            ("origin", "http://[::1]:4519"),
            ("x-ruddr-request", "1")
        ])));
        assert!(is_same_origin_mutation(&headers(&[
            ("host", "box"),
            ("origin", "http://BOX:80"),
            ("x-ruddr-request", "1")
        ])));
        assert!(!is_same_origin_mutation(&headers(&[
            ("host", "box:80"),
            ("origin", "http://box:80"),
            ("x-ruddr-request", "1")
        ])));
    }

    #[test]
    fn reads_cookies_bearer_tokens_and_queries() {
        let token = "t".repeat(40);
        assert!(is_authorized(&headers(&[("cookie", &format!("a=b; ruddr_web={token}"))]), &token));
        assert!(is_authorized(&headers(&[("authorization", &format!("bearer  {token}"))]), &token));
        assert!(is_authorized(&headers(&[("authorization", &token)]), &token));
        assert!(!is_authorized(&headers(&[("cookie", "ruddr_web=%")]), &token));
        assert!(!is_authorized(&headers(&[]), &token));
        assert_eq!(cookie_value(&headers(&[("cookie", "x=a%20b=c")]), "x").as_deref(), Some("a b=c"));
        assert_eq!(query_param(Some("dir=%2Ftmp%2Fa+b&force"), "dir").as_deref(), Some("/tmp/a b"));
        assert_eq!(query_param(Some("token=%zz"), "token").as_deref(), Some("%zz"));
        assert!(has_query_param(Some("dir=x&force"), "force"));
        assert!(!has_query_param(None, "force"));
        assert_eq!(encode_component("a b/é"), "a%20b%2F%C3%A9");
    }
}
