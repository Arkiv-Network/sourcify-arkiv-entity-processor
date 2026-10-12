//! Negotiate the available representations, then let Tower HTTP stream the codec.
//! Tower handles compression and framing but does not handle wildcard acceptance
//! or reject requests that forbid identity as well as every enabled coding.
use axum::{
    body::HttpBody,
    extract::Request,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

#[derive(Clone, Copy)]
pub(crate) struct Json;
impl tower_http::compression::predicate::Predicate for Json {
    fn should_compress<B: HttpBody>(&self, response: &Response<B>) -> bool {
        response.body().size_hint().exact() != Some(0)
            && response
                .headers()
                .get(header::CONTENT_TYPE)
                .is_some_and(|value| {
                    value.as_bytes().split(|&b| b == b';').next()
                        == Some(b"application/json".as_slice())
                })
    }
}

pub(crate) async fn negotiate(mut request: Request, next: Next) -> Response {
    let mut response = match select(request.headers()) {
        Ok(encoding) => {
            // A single normalized choice prevents the inner middleware's identity
            // fallback or unsupported wildcard handling from changing our choice.
            request
                .headers_mut()
                .insert(header::ACCEPT_ENCODING, HeaderValue::from_static(encoding));
            next.run(request).await
        }
        // No representation was accepted (406), or the header was malformed (400).
        // An empty response does not send a body using a forbidden content coding.
        Err(status) => status.into_response(),
    };
    if !response
        .headers()
        .get_all(header::VARY)
        .iter()
        .any(|value| {
            value.to_str().is_ok_and(|value| {
                value.split(',').any(|token| {
                    token.trim() == "*" || token.trim().eq_ignore_ascii_case("accept-encoding")
                })
            })
        })
    {
        response
            .headers_mut()
            .append(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    response
}

fn select(headers: &HeaderMap) -> Result<&'static str, StatusCode> {
    let mut gzip = None;
    let mut zstd = None;
    let mut identity = None;
    let mut wildcard = None;
    for value in headers.get_all(header::ACCEPT_ENCODING) {
        for item in value
            .to_str()
            .map_err(|_| StatusCode::BAD_REQUEST)?
            .split(',')
        {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            let mut parts = item.split(';');
            let coding = parts.next().unwrap().trim();
            let weight = match parts.next() {
                Some(parameter) => quality(parameter.trim()).ok_or(StatusCode::BAD_REQUEST)?,
                None => 1000,
            };
            if parts.next().is_some() {
                return Err(StatusCode::BAD_REQUEST);
            }
            let target =
                if coding.eq_ignore_ascii_case("gzip") || coding.eq_ignore_ascii_case("x-gzip") {
                    &mut gzip
                } else if coding.eq_ignore_ascii_case("zstd") {
                    &mut zstd
                } else if coding.eq_ignore_ascii_case("identity") {
                    &mut identity
                } else if coding == "*" {
                    &mut wildcard
                } else {
                    continue;
                };
            // Conflicting duplicates have no useful preference order. Preserve
            // the strictest declaration, including explicit rejection (q=0).
            *target = Some(target.map_or(weight, |previous: u16| previous.min(weight)));
        }
    }
    let gzip = gzip.or(wildcard).unwrap_or(0);
    let zstd = zstd.or(wildcard).unwrap_or(0);
    let identity_allowed = identity.map_or(wildcard != Some(0), |quality| quality > 0);
    // Implicit identity is a fallback. Explicit identity participates in quality
    // ranking; ties prefer zstd, then gzip, then identity.
    let choices = [
        (identity.unwrap_or(0), "identity"),
        (gzip, "gzip"),
        (zstd, "zstd"),
    ];
    if let Some((_, coding)) = choices
        .into_iter()
        .filter(|(quality, _)| *quality > 0)
        .max_by_key(|(quality, _)| *quality)
    {
        Ok(coding)
    } else if identity_allowed {
        Ok("identity")
    } else {
        Err(StatusCode::NOT_ACCEPTABLE)
    }
}

/// RFC 9110 quality values, represented exactly as thousandths rather than floats.
fn quality(parameter: &str) -> Option<u16> {
    let (name, value) = parameter.split_once('=')?;
    if !name.eq_ignore_ascii_case("q") {
        return None;
    }
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if fraction.len() > 3 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    match whole {
        "0" => Some(fraction.parse::<u16>().unwrap_or(0) * 10_u16.pow(3 - fraction.len() as u32)),
        "1" if fraction.bytes().all(|byte| byte == b'0') => Some(1000),
        _ => None,
    }
}
