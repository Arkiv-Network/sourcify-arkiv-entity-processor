//! Minimal S3 client for `upload`: AWS Signature V4 over path-style URLs and a blocking rustls
//! HTTP client. Only what a never-overwriting multipart upload and its read-back need.
//!
//! The key pair is only used for signing: it never appears in URLs, output, errors or `Debug`.
//! Error responses are reduced to their S3 `<Code>` and `<Message>` elements.
use anyhow::{bail, ensure, Context, Result};
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fmt,
    fmt::Write as _,
    fs,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};
use ureq::http::{self, Method};

/// SHA-256 of an empty payload.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
/// Attempts per request; see [`S3Error::retryable`].
const ATTEMPTS: u32 = 5;
/// Variables read from the credentials file or the environment.
const VARIABLES: [&str; 4] = [
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_ENDPOINT_URL_S3",
    "AWS_DEFAULT_REGION",
];

/// Key pair, endpoint and region of one S3-compatible service. `Debug` redacts the key pair.
pub struct Credentials {
    access_key_id: String,
    secret_access_key: String,
    /// Normalized `scheme://host[:port]` (default port dropped, no trailing slash).
    endpoint: String,
    region: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &"<redacted>")
            .field("secret_access_key", &"<redacted>")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .finish()
    }
}

impl Credentials {
    /// Reads the env-style `file` when given (only that file), else the process environment.
    pub fn load(file: Option<&Path>) -> Result<Self> {
        let Some(path) = file else {
            let vars = VARIABLES
                .iter()
                .filter_map(|&k| std::env::var(k).ok().map(|v| (k.to_owned(), v)))
                .collect();
            return Self::from_vars(&vars, "the environment");
        };
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading credentials file {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if fs::metadata(path)?.permissions().mode() & 0o077 != 0 {
                eprintln!(
                    "{}",
                    json!({"warning": format!("credentials file {} is readable by group/others; chmod 600 it", path.display())})
                );
            }
        }
        let vars = parse_env_file(&text)
            .with_context(|| format!("credentials file {}", path.display()))?;
        Self::from_vars(&vars, &path.display().to_string())
    }

    fn from_vars(vars: &HashMap<String, String>, source: &str) -> Result<Self> {
        let get = |name: &str| vars.get(name).map(|v| v.trim()).filter(|v| !v.is_empty());
        let required =
            |name: &str| get(name).with_context(|| format!("{name} is not set in {source}"));
        let access_key_id = required("AWS_ACCESS_KEY_ID")?;
        let secret_access_key = required("AWS_SECRET_ACCESS_KEY")?;
        // Never echo the values themselves.
        for (name, value) in [
            ("AWS_ACCESS_KEY_ID", access_key_id),
            ("AWS_SECRET_ACCESS_KEY", secret_access_key),
        ] {
            ensure!(
                value.bytes().all(|b| b.is_ascii_graphic()),
                "{name} in {source} contains whitespace or non-ASCII characters"
            );
        }
        let region = get("AWS_DEFAULT_REGION").unwrap_or("auto");
        ensure!(
            region
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "AWS_DEFAULT_REGION in {source} is not a valid region name"
        );
        Ok(Self {
            access_key_id: access_key_id.to_owned(),
            secret_access_key: secret_access_key.to_owned(),
            endpoint: parse_endpoint(required("AWS_ENDPOINT_URL_S3")?)
                .with_context(|| format!("AWS_ENDPOINT_URL_S3 in {source}"))?,
            region: region.to_owned(),
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn host(&self) -> &str {
        self.endpoint.split_once("://").map_or("", |(_, host)| host)
    }

    /// Signature V4 `Authorization` value. `headers` are (lowercase name, value) pairs, all of
    /// them signed; `uri` and `query` are already in canonical (URI-encoded) form.
    fn authorization(
        &self,
        method: &str,
        uri: &str,
        query: &str,
        headers: &[(&str, &str)],
        payload_sha256: &str,
        amz_date: &str,
    ) -> String {
        let mut headers = headers.to_vec();
        headers.sort();
        let mut canonical_headers = String::new();
        for (name, value) in &headers {
            // Trimmed, with runs of spaces collapsed.
            let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
            let _ = writeln!(canonical_headers, "{name}:{value}");
        }
        let signed = headers
            .iter()
            .map(|(n, _)| *n)
            .collect::<Vec<_>>()
            .join(";");
        let canonical =
            format!("{method}\n{uri}\n{query}\n{canonical_headers}\n{signed}\n{payload_sha256}");
        let date = &amz_date[..8];
        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            hex::encode(Sha256::digest(canonical.as_bytes()))
        );
        let mut key = hmac(
            format!("AWS4{}", self.secret_access_key).as_bytes(),
            date.as_bytes(),
        );
        for part in [self.region.as_str(), "s3", "aws4_request"] {
            key = hmac(&key, part.as_bytes());
        }
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope},SignedHeaders={signed},Signature={}",
            self.access_key_id,
            hex::encode(hmac(&key, to_sign.as_bytes()))
        )
    }
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes keys of any length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// Parses `KEY=VALUE` lines: optional leading `export `, blank lines and `#` comment lines
/// ignored, a value may be wrapped in single or double quotes (taken literally), and an unquoted
/// value ends at ` #`. Errors name the line number only, never its content.
pub fn parse_env_file(text: &str) -> Result<HashMap<String, String>> {
    let mut vars = HashMap::new();
    for (n, line) in text.lines().enumerate() {
        let n = n + 1;
        let line = line.trim_start_matches('\u{feff}').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = match line.strip_prefix("export") {
            Some(rest) if rest.starts_with([' ', '\t']) => rest.trim_start(),
            _ => line,
        };
        let Some((name, value)) = line.split_once('=') else {
            bail!("line {n}: expected KEY=VALUE");
        };
        let name = name.trim_end();
        ensure!(
            !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "line {n}: invalid variable name"
        );
        let value = value.trim_start();
        let value = match value.chars().next() {
            Some(quote @ ('"' | '\'')) => {
                let Some((inner, rest)) = value[1..].split_once(quote) else {
                    bail!("line {n}: unterminated quote");
                };
                let rest = rest.trim_start();
                ensure!(
                    rest.is_empty() || rest.starts_with('#'),
                    "line {n}: unexpected text after the closing quote"
                );
                inner
            }
            _ => {
                let end = value
                    .char_indices()
                    .find(|&(i, c)| {
                        c == '#' && value[..i].ends_with(|p: char| p.is_ascii_whitespace())
                    })
                    .map_or(value.len(), |(i, _)| i);
                value[..end].trim_end()
            }
        };
        vars.insert(name.to_owned(), value.to_owned());
    }
    Ok(vars)
}

/// `scheme://host[:port]` with an optional trailing slash, normalized to lowercase without the
/// scheme's default port. The value is never echoed in errors.
fn parse_endpoint(url: &str) -> Result<String> {
    let url = url.strip_suffix('/').unwrap_or(url);
    let (scheme, authority) = url
        .split_once("://")
        .context("must start with https:// (or http://)")?;
    let scheme = scheme.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "https" => ":443",
        "http" => ":80",
        _ => bail!("must start with https:// (or http://)"),
    };
    ensure!(
        !authority.is_empty()
            && authority
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-.:[]".contains(&b)),
        "must be scheme://host[:port] without path, query or user info"
    );
    let authority = authority.to_ascii_lowercase();
    let authority = authority.strip_suffix(default_port).unwrap_or(&authority);
    Ok(format!("{scheme}://{authority}"))
}

/// S3 URI encoding: unreserved bytes (A-Z a-z 0-9 - _ . ~) stay, everything else becomes %XX;
/// `/` stays too when `keep_slash` (object key paths).
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) || (keep_slash && b == b'/') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Canonical query string: encoded pairs sorted by name, `name=` for empty values.
fn canonical_query(pairs: &[(&str, &str)]) -> String {
    let mut encoded: Vec<_> = pairs
        .iter()
        .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
        .collect();
    encoded.sort();
    let pairs: Vec<_> = encoded.iter().map(|(k, v)| format!("{k}={v}")).collect();
    pairs.join("&")
}

/// Bucket names as S3 and R2 accept them: 3-63 characters of a-z, 0-9, `.` and `-`.
pub fn check_bucket(bucket: &str) -> Result<()> {
    ensure!(
        (3..=63).contains(&bucket.len())
            && bucket
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-'),
        "invalid bucket name {bucket:?}: use 3-63 characters of a-z, 0-9, '.' and '-'"
    );
    Ok(())
}

/// Object keys: 1-1024 bytes, no control characters and no empty, `.` or `..` path segments
/// (those may be normalized away by HTTP clients or servers).
pub fn check_key(key: &str) -> Result<()> {
    ensure!(
        !key.is_empty() && key.len() <= 1024,
        "object key must be 1-1024 bytes"
    );
    ensure!(
        !key.chars().any(char::is_control),
        "object key contains control characters"
    );
    ensure!(
        key.split('/').all(|s| !matches!(s, "" | "." | "..")),
        "object key {key:?} has an empty, '.' or '..' segment (leading/trailing/double '/')"
    );
    Ok(())
}

/// A failed S3 request, reduced to what is safe to print.
#[derive(Debug)]
pub enum S3Error {
    /// No (complete) response: connection, TLS, timeout or body read failure. The request may or
    /// may not have taken effect.
    Transport(String),
    /// An error status, or an `<Error>` document in a 200 response.
    Status {
        status: u16,
        code: String,
        message: String,
    },
    /// SIGINT/SIGTERM arrived before (another) attempt.
    Interrupted,
}

impl S3Error {
    /// Network errors, HTTP 5xx, 429 and throttling/timeout codes are worth another attempt.
    pub fn retryable(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::Status { status, code, .. } => {
                *status >= 500
                    || *status == 429
                    || matches!(
                        code.as_str(),
                        "RequestTimeout" | "SlowDown" | "InternalError" | "ServiceUnavailable"
                    )
            }
            Self::Interrupted => false,
        }
    }

    /// The HTTP status and S3 error code, if the server answered.
    pub fn status(&self) -> Option<(u16, &str)> {
        match self {
            Self::Status { status, code, .. } => Some((*status, code)),
            _ => None,
        }
    }
}

impl fmt::Display for S3Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "network error: {e}"),
            Self::Status {
                status,
                code,
                message,
            } => {
                write!(f, "HTTP {status}")?;
                if !code.is_empty() {
                    write!(f, " {code}")?;
                }
                if !message.is_empty() {
                    write!(f, ": {message}")?;
                }
                Ok(())
            }
            Self::Interrupted => write!(f, "interrupted"),
        }
    }
}

impl std::error::Error for S3Error {}

/// Runs `request` up to 5 times with exponential backoff (1, 2, 4, 8 s) while it fails with a
/// retryable error; `stop` (SIGINT/SIGTERM) ends the retries early.
pub fn retry<T>(
    what: &str,
    stop: Option<&AtomicBool>,
    mut request: impl FnMut() -> Result<T, S3Error>,
) -> Result<T, S3Error> {
    let stopped = || stop.is_some_and(|s| s.load(Ordering::Relaxed));
    let mut delay = Duration::from_secs(1);
    let mut attempt = 1;
    loop {
        if stopped() {
            return Err(S3Error::Interrupted);
        }
        match request() {
            Err(e) if e.retryable() && attempt < ATTEMPTS => {
                eprintln!(
                    "{}",
                    json!({"retry": what, "attempt": attempt, "error": e.to_string(), "delaySeconds": delay.as_secs()})
                );
                let until = Instant::now() + delay;
                while Instant::now() < until && !stopped() {
                    thread::sleep(Duration::from_millis(100));
                }
                delay *= 2;
                attempt += 1;
            }
            result => return result,
        }
    }
}

/// Text of the first `<tag>` element (XML entities decoded), shortened for display.
fn xml_text(body: &str, tag: &str) -> Option<String> {
    let start = body.find(&format!("<{tag}>"))? + tag.len() + 2;
    let end = start + body[start..].find(&format!("</{tag}>"))?;
    let text = body[start..end]
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&");
    Some(text.chars().filter(|c| !c.is_control()).take(300).collect())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Time allowed to send or receive a body: 2 minutes plus 2 s per MiB (>= 0.5 MiB/s).
fn body_timeout(bytes: u64) -> Duration {
    Duration::from_secs(120 + 2 * bytes.div_ceil(1 << 20))
}

/// Size and `sha256` user metadata of an existing object.
#[derive(Debug)]
pub struct ObjectInfo {
    pub size: u64,
    pub sha256: Option<String>,
}

/// Blocking client for one endpoint; shareable between threads.
pub struct Client {
    agent: ureq::Agent,
    credentials: Credentials,
}

impl Client {
    /// `connections`: idle connections kept for reuse (the number of parallel requests).
    pub fn new(credentials: Credentials, connections: usize) -> Self {
        let agent = ureq::Agent::config_builder()
            // Error statuses come back as responses, so their <Code>/<Message> can be read.
            .http_status_as_error(false)
            // S3 path-style requests never redirect; a 3xx is reported as an error.
            .max_redirects(0)
            .max_redirects_will_error(false)
            .user_agent(concat!(
                "arkiv-entity-processor/",
                env!("CARGO_PKG_VERSION")
            ))
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_send_request(Some(Duration::from_secs(60)))
            .timeout_recv_response(Some(Duration::from_secs(600)))
            .max_idle_connections(connections.max(4))
            .max_idle_connections_per_host(connections.max(4))
            .build()
            .into();
        Self { agent, credentials }
    }

    /// One signed path-style request (`{endpoint}/{bucket}/{key}?{query}`), without retries.
    /// `headers` (lowercase names) are signed; non-2xx statuses become [`S3Error::Status`].
    #[allow(clippy::too_many_arguments)]
    fn send(
        &self,
        method: Method,
        bucket: &str,
        key: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
        body: impl ureq::AsSendBody,
        payload_sha256: &str,
        body_bytes: u64,
    ) -> Result<http::Response<ureq::Body>, S3Error> {
        let path = format!("/{bucket}/{}", uri_encode(key, true));
        let mut url = format!("{}{path}", self.credentials.endpoint);
        for (i, (name, value)) in query.iter().enumerate() {
            url.push(if i == 0 { '?' } else { '&' });
            url.push_str(&uri_encode(name, false));
            if !value.is_empty() {
                url.push('=');
                url.push_str(&uri_encode(value, false));
            }
        }
        let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let mut signed = vec![
            ("host", self.credentials.host()),
            ("x-amz-content-sha256", payload_sha256),
            ("x-amz-date", amz_date.as_str()),
        ];
        signed.extend_from_slice(headers);
        let authorization = self.credentials.authorization(
            method.as_str(),
            &path,
            &canonical_query(query),
            &signed,
            payload_sha256,
            &amz_date,
        );
        let mut request = http::Request::builder().method(method).uri(url);
        for (name, value) in signed {
            request = request.header(name, value);
        }
        let request = request
            .header("authorization", authorization)
            .body(body)
            .map_err(|e| S3Error::Transport(format!("invalid request: {e}")))?;
        let timeout = Some(body_timeout(body_bytes));
        let request = self
            .agent
            .configure_request(request)
            .timeout_send_body(timeout)
            .timeout_recv_body(timeout)
            .build();
        let response = self
            .agent
            .run(request)
            .map_err(|e| S3Error::Transport(e.to_string()))?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let body = read_small(response).unwrap_or_default();
        Err(S3Error::Status {
            status,
            code: xml_text(&body, "Code").unwrap_or_default(),
            message: xml_text(&body, "Message").unwrap_or_default(),
        })
    }

    /// `None` if the key does not exist (404).
    pub fn head(&self, bucket: &str, key: &str) -> Result<Option<ObjectInfo>, S3Error> {
        let response = match self.send(Method::HEAD, bucket, key, &[], &[], (), EMPTY_SHA256, 0) {
            Err(S3Error::Status { status: 404, .. }) => return Ok(None),
            other => other?,
        };
        let header = |name| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let size = header("content-length")
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| malformed("HEAD response without a valid Content-Length"))?;
        Ok(Some(ObjectInfo {
            size,
            sha256: header("x-amz-meta-sha256").map(|v| v.to_ascii_lowercase()),
        }))
    }

    /// CreateMultipartUpload with `x-amz-meta-sha256`; returns the upload ID.
    pub fn create_multipart(
        &self,
        bucket: &str,
        key: &str,
        content_type: &str,
        sha256: &str,
    ) -> Result<String, S3Error> {
        let headers = [
            ("content-type", content_type),
            ("x-amz-meta-sha256", sha256),
        ];
        let response = self.send(
            Method::POST,
            bucket,
            key,
            &[("uploads", "")],
            &headers,
            (),
            EMPTY_SHA256,
            0,
        )?;
        let body = read_small(response)?;
        xml_text(&body, "UploadId")
            .filter(|id| !id.is_empty())
            .ok_or_else(|| malformed("CreateMultipartUpload response without UploadId"))
    }

    /// UploadPart; `sha256` (hex SHA-256 of `data`) is sent as x-amz-content-sha256, so the
    /// server rejects corrupted bodies. Returns the part's ETag.
    pub fn upload_part(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        number: u64,
        data: &[u8],
        sha256: &str,
    ) -> Result<String, S3Error> {
        let number = number.to_string();
        let query = [("partNumber", number.as_str()), ("uploadId", upload_id)];
        let response = self.send(
            Method::PUT,
            bucket,
            key,
            &query,
            &[],
            data,
            sha256,
            data.len() as u64,
        )?;
        let etag = response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
            .map(str::to_owned);
        // Drain the (empty) body so the connection can be reused.
        let _ = read_small(response);
        etag.ok_or_else(|| malformed("UploadPart response without ETag"))
    }

    /// CompleteMultipartUpload with `If-None-Match: *`: fails (412) instead of replacing an
    /// object that exists by then. A 200 response carrying an `<Error>` document is a failure.
    pub fn complete_multipart(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        etags: &[String],
    ) -> Result<(), S3Error> {
        let mut xml = String::from(
            "<CompleteMultipartUpload xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">",
        );
        for (i, etag) in etags.iter().enumerate() {
            let _ = write!(
                xml,
                "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
                i + 1,
                xml_escape(etag)
            );
        }
        xml.push_str("</CompleteMultipartUpload>");
        let sha256 = hex::encode(Sha256::digest(xml.as_bytes()));
        let headers = [("content-type", "application/xml"), ("if-none-match", "*")];
        let response = self.send(
            Method::POST,
            bucket,
            key,
            &[("uploadId", upload_id)],
            &headers,
            xml.as_bytes(),
            &sha256,
            xml.len() as u64,
        )?;
        let status = response.status().as_u16();
        let body = read_small(response)?;
        if body.contains("<Error>") {
            return Err(S3Error::Status {
                status,
                code: xml_text(&body, "Code").unwrap_or_default(),
                message: xml_text(&body, "Message").unwrap_or_default(),
            });
        }
        Ok(())
    }

    /// AbortMultipartUpload: discards the uploaded parts.
    pub fn abort_multipart(&self, bucket: &str, key: &str, upload_id: &str) -> Result<(), S3Error> {
        let response = self.send(
            Method::DELETE,
            bucket,
            key,
            &[("uploadId", upload_id)],
            &[],
            (),
            EMPTY_SHA256,
            0,
        )?;
        let _ = read_small(response);
        Ok(())
    }

    /// GET of the inclusive byte range `start..=end`; with `if_match` (an ETag) the object must
    /// be unchanged. The caller streams the body.
    pub fn get_range(
        &self,
        bucket: &str,
        key: &str,
        start: u64,
        end: u64,
        if_match: Option<&str>,
    ) -> Result<http::Response<ureq::Body>, S3Error> {
        let range = format!("bytes={start}-{end}");
        let mut headers = vec![("range", range.as_str())];
        if let Some(etag) = if_match {
            headers.push(("if-match", etag));
        }
        self.send(
            Method::GET,
            bucket,
            key,
            &[],
            &headers,
            (),
            EMPTY_SHA256,
            end + 1 - start,
        )
    }
}

fn malformed(message: &str) -> S3Error {
    S3Error::Status {
        status: 200,
        code: "MalformedResponse".into(),
        message: message.into(),
    }
}

/// A response body of at most 1 MiB as text (XML documents).
fn read_small(response: http::Response<ureq::Body>) -> Result<String, S3Error> {
    let mut body = response.into_body();
    body.with_config()
        .limit(1 << 20)
        .lossy_utf8(true)
        .read_to_string()
        .map_err(|e| S3Error::Transport(format!("reading response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example key pair from the AWS Signature V4 documentation.
    fn example() -> Credentials {
        Credentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            endpoint: "https://examplebucket.s3.amazonaws.com".into(),
            region: "us-east-1".into(),
        }
    }

    const AUTH: &str = "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,SignedHeaders=";

    #[test]
    fn signature_v4_matches_aws_documented_examples() {
        // "Signature Calculations for the Authorization Header", Amazon S3 API reference.
        let c = example();
        let host = ("host", "examplebucket.s3.amazonaws.com");
        let date = ("x-amz-date", "20130524T000000Z");
        // GET Object with a Range header.
        let get = c.authorization(
            "GET",
            "/test.txt",
            "",
            &[
                host,
                ("range", "bytes=0-9"),
                ("x-amz-content-sha256", EMPTY_SHA256),
                date,
            ],
            EMPTY_SHA256,
            "20130524T000000Z",
        );
        assert_eq!(get, format!("{AUTH}host;range;x-amz-content-sha256;x-amz-date,Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"));
        // PUT Object: a key that needs encoding, unsorted headers, a real payload hash.
        let payload = hex::encode(Sha256::digest(b"Welcome to Amazon S3."));
        let put = c.authorization(
            "PUT",
            &format!("/{}", uri_encode("test$file.text", true)),
            "",
            &[
                ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
                date,
                ("date", "Fri, 24 May 2013 00:00:00 GMT"),
                host,
                ("x-amz-content-sha256", &payload),
            ],
            &payload,
            "20130524T000000Z",
        );
        assert_eq!(put, format!("{AUTH}date;host;x-amz-content-sha256;x-amz-date;x-amz-storage-class,Signature=98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"));
        // GET Bucket (list objects): query parameters, sorted and encoded.
        let headers = [host, ("x-amz-content-sha256", EMPTY_SHA256), date];
        let list = c.authorization(
            "GET",
            "/",
            &canonical_query(&[("prefix", "J"), ("max-keys", "2")]),
            &headers,
            EMPTY_SHA256,
            "20130524T000000Z",
        );
        assert_eq!(list, format!("{AUTH}host;x-amz-content-sha256;x-amz-date,Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"));
        // GET Bucket lifecycle: a query parameter without value signs as "lifecycle=".
        let lifecycle = c.authorization(
            "GET",
            "/",
            &canonical_query(&[("lifecycle", "")]),
            &headers,
            EMPTY_SHA256,
            "20130524T000000Z",
        );
        assert_eq!(lifecycle, format!("{AUTH}host;x-amz-content-sha256;x-amz-date,Signature=fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543"));
    }

    #[test]
    fn uri_encoding_follows_s3_rules() {
        assert_eq!(
            uri_encode("a b/c+d~e_f.g-h/ü*", true),
            "a%20b/c%2Bd~e_f.g-h/%C3%BC%2A"
        );
        assert_eq!(uri_encode("a/b=c&d", false), "a%2Fb%3Dc%26d");
        assert_eq!(
            canonical_query(&[("uploadId", "x/y+z="), ("partNumber", "10")]),
            "partNumber=10&uploadId=x%2Fy%2Bz%3D"
        );
    }

    #[test]
    fn env_file_parsing() {
        let vars = parse_env_file(
            "\u{feff}# R2 credentials\n\
             export AWS_ACCESS_KEY_ID=abc123\n\
             \n\
             AWS_SECRET_ACCESS_KEY = \"s3cr#t value\"  # comment\n\
             AWS_ENDPOINT_URL_S3='https://acct.r2.cloudflarestorage.com/'\r\n\
             AWS_DEFAULT_REGION=auto # trailing comment\n\
             PLAIN=a#b\n\
             EMPTY=\n\
             exported=1\n",
        )
        .unwrap();
        assert_eq!(vars["AWS_ACCESS_KEY_ID"], "abc123");
        assert_eq!(vars["AWS_SECRET_ACCESS_KEY"], "s3cr#t value");
        assert_eq!(
            vars["AWS_ENDPOINT_URL_S3"],
            "https://acct.r2.cloudflarestorage.com/"
        );
        assert_eq!(vars["AWS_DEFAULT_REGION"], "auto");
        assert_eq!(vars["PLAIN"], "a#b");
        assert_eq!(vars["EMPTY"], "");
        assert_eq!(vars["exported"], "1");
        for (text, error) in [
            ("A=1\nSECRETVALUE\n", "line 2: expected KEY=VALUE"),
            ("A=\"SECRETVALUE\n", "line 1: unterminated quote"),
            ("A='SECRET'VALUE\n", "line 1: unexpected text"),
            ("BAD-NAME=SECRETVALUE\n", "line 1: invalid variable name"),
        ] {
            let message = format!("{:#}", parse_env_file(text).unwrap_err());
            assert!(message.contains(error), "{message}");
            assert!(!message.contains("SECRET"), "{message}");
        }
    }

    #[test]
    fn credentials_never_show_the_key_pair() {
        let vars: HashMap<String, String> = [
            ("AWS_ACCESS_KEY_ID", "AKIDSECRETID"),
            ("AWS_SECRET_ACCESS_KEY", "SUPERSECRET"),
            (
                "AWS_ENDPOINT_URL_S3",
                "HTTPS://Acct.R2.cloudflarestorage.com:443/",
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let c = Credentials::from_vars(&vars, "test").unwrap();
        assert_eq!(c.endpoint, "https://acct.r2.cloudflarestorage.com");
        assert_eq!(
            (c.host(), c.region.as_str()),
            ("acct.r2.cloudflarestorage.com", "auto")
        );
        let debug = format!("{c:?}");
        assert!(!debug.contains("SECRET"), "{debug}");
        let mut bad = vars.clone();
        bad.insert("AWS_SECRET_ACCESS_KEY".into(), "SUPER SECRET".into());
        let message = Credentials::from_vars(&bad, "test")
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("AWS_SECRET_ACCESS_KEY") && !message.contains("SUPER"),
            "{message}"
        );
        bad.remove("AWS_SECRET_ACCESS_KEY");
        assert!(Credentials::from_vars(&bad, "test").is_err());
    }

    #[test]
    fn endpoints_are_normalized_and_validated() {
        assert_eq!(
            parse_endpoint("http://127.0.0.1:9900").unwrap(),
            "http://127.0.0.1:9900"
        );
        assert_eq!(
            parse_endpoint("http://localhost:80/").unwrap(),
            "http://localhost"
        );
        assert_eq!(parse_endpoint("https://h:8443").unwrap(), "https://h:8443");
        for bad in [
            "h.example",
            "ftp://h",
            "https://",
            "https://h/path",
            "https://u:p@h",
        ] {
            assert!(parse_endpoint(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn bucket_and_key_rules() {
        check_bucket("arkiv-seeds.v1").unwrap();
        for bad in ["ab", "Upper", "under_score", &"a".repeat(64)] {
            assert!(check_bucket(bad).is_err(), "{bad}");
        }
        check_key("sourcify/2026-10-05/bundle.tar.zst").unwrap();
        for bad in ["", "/a", "a/", "a//b", "a/./b", "../a", "a\nb"] {
            assert!(check_key(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn error_documents_and_retry_classes() {
        let body = "<?xml version=\"1.0\"?><Error><Code>SlowDown</Code><Message>Please &amp; reduce &lt;rate&gt;</Message><RequestId>1</RequestId><StringToSign>x</StringToSign></Error>";
        assert_eq!(xml_text(body, "Code").unwrap(), "SlowDown");
        assert_eq!(xml_text(body, "Message").unwrap(), "Please & reduce <rate>");
        let status = |status, code: &str| S3Error::Status {
            status,
            code: code.into(),
            message: String::new(),
        };
        assert!(S3Error::Transport("reset".into()).retryable());
        assert!(status(503, "").retryable());
        assert!(status(429, "").retryable());
        assert!(status(400, "RequestTimeout").retryable());
        assert!(status(200, "InternalError").retryable());
        assert!(!status(403, "AccessDenied").retryable());
        assert!(!status(412, "PreconditionFailed").retryable());
        assert!(!S3Error::Interrupted.retryable());
        assert_eq!(
            status(403, "AccessDenied").to_string(),
            "HTTP 403 AccessDenied"
        );
        let mut calls = 0;
        let stop = AtomicBool::new(false);
        let result: Result<(), _> = retry("test", Some(&stop), || {
            calls += 1;
            Err(status(404, "NoSuchKey"))
        });
        assert!(result.is_err());
        assert_eq!(calls, 1);
        stop.store(true, Ordering::Relaxed);
        assert!(matches!(
            retry("test", Some(&stop), || Ok::<_, S3Error>(())),
            Err(S3Error::Interrupted)
        ));
    }
}
