//! Minimal HTTP/1.1 front end for the runtime's single HTTP port.
//!
//! One request per connection, no keep-alive, no TLS. Requests are parsed into
//! a [`Request`] with a byte body, routed by `(method, path)` through
//! [`route`], and answered with a [`Response`] carrying a byte body.
//!
//! Routes today: `POST /` and `POST /mcp` -> MCP. `/pair` and `/admin/*` are
//! reserved for operator endpoints (T6) and plug in as new [`Route`] variants.
//! Unknown paths get a bare 404, known paths with the wrong method a bare 405
//! with an `Allow` header (no JSON-RPC body in either case).

use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt};

/// Cap on a whole request (headers + body); larger requests are dropped.
pub const MAX_REQUEST: usize = 64 * 1024;

/// Per-connection budget for receiving a complete request.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// A parsed HTTP request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    /// Path without the query string.
    pub path: String,
    /// Raw query string (after `?`), empty if absent.
    pub query: String,
    /// Credentials of an `Authorization: Bearer` header.
    pub bearer: Option<String>,
    pub body: Vec<u8>,
}

/// An HTTP response with a byte body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    /// Value of the `Allow` header (405 responses).
    pub allow: Option<&'static str>,
}

impl Response {
    pub fn json(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            body: body.into(),
            allow: None,
        }
    }

    fn empty(status: u16) -> Self {
        Self {
            status,
            content_type: "text/plain",
            body: Vec::new(),
            allow: None,
        }
    }

    pub fn bad_request() -> Self {
        Self::empty(400)
    }

    pub fn not_implemented() -> Self {
        Self::empty(501)
    }

    pub fn not_found() -> Self {
        Self::empty(404)
    }

    pub fn method_not_allowed(allow: &'static str) -> Self {
        Self {
            allow: Some(allow),
            ..Self::empty(405)
        }
    }

    /// An operator-facing error (`/pair`, `/admin/*`) as `{"code","hint"}`.
    pub fn operator_error(status: u16, err: &OperatorError) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(err).unwrap_or_default(),
            allow: None,
        }
    }

    /// Serialize to an HTTP/1.1 message (always `Connection: close`).
    pub fn to_bytes(&self) -> Vec<u8> {
        let reason = match self.status {
            200 => "OK",
            400 => "Bad Request",
            404 => "Not Found",
            405 => "Method Not Allowed",
            501 => "Not Implemented",
            _ => "Error",
        };
        let mut head = format!(
            "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            self.status,
            self.content_type,
            self.body.len()
        );
        if let Some(allow) = self.allow {
            head.push_str(&format!("Allow: {allow}\r\n"));
        }
        head.push_str("\r\n");
        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

/// Operator-endpoint error codes. Deliberately separate from the agent
/// `ERROR_CODES` set: these never appear on the model surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OperatorCode {
    PairingClosed,
    PairCodeInvalid,
    NotAdmin,
    NotInstalled,
    UpdateFailed,
}

/// Body of an operator-endpoint error: `{"code","hint"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperatorError {
    pub code: OperatorCode,
    pub hint: String,
}

impl OperatorError {
    pub fn new(code: OperatorCode, hint: impl Into<String>) -> Self {
        Self {
            code,
            hint: hint.into(),
        }
    }
}

/// Where a request goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Mcp,
    Respond(Response),
}

/// Route by `(method, path)`.
pub fn route(method: &str, path: &str) -> Route {
    match (method, path) {
        ("POST", "/" | "/mcp") => Route::Mcp,
        (_, "/" | "/mcp") => Route::Respond(Response::method_not_allowed("POST")),
        _ => Route::Respond(Response::not_found()),
    }
}

/// Value of `key` in a raw query string (`a=1&b=2`), without percent-decoding.
pub fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter_map(|kv| kv.split_once('=').or(Some((kv, ""))))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

/// Why a request could not be read.
#[derive(Debug, PartialEq, Eq)]
pub enum ReadError {
    /// Peer closed or errored before a full request arrived.
    Closed,
    /// Request exceeded [`MAX_REQUEST`].
    TooLarge,
    /// Bad request line, malformed header, or ambiguous framing/credentials
    /// (duplicate or non-numeric `Content-Length`, duplicate `Authorization`).
    Malformed,
    /// `Transfer-Encoding` present; only `Content-Length` bodies are supported.
    NotImplemented,
    /// No complete request within the timeout.
    Timeout,
}

/// Read one request, giving up after `timeout` in total.
pub async fn read_request<R: AsyncRead + Unpin>(
    stream: &mut R,
    timeout: Duration,
) -> Result<Request, ReadError> {
    tokio::time::timeout(timeout, read_request_inner(stream))
        .await
        .unwrap_or(Err(ReadError::Timeout))
}

async fn read_request_inner<R: AsyncRead + Unpin>(stream: &mut R) -> Result<Request, ReadError> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];

    // A single read may return a partial segment: loop until the header
    // terminator, then until Content-Length body bytes have arrived.
    let header_end = loop {
        let n = stream.read(&mut tmp).await.map_err(|_| ReadError::Closed)?;
        if n == 0 {
            return Err(ReadError::Closed);
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > MAX_REQUEST {
            return Err(ReadError::TooLarge);
        }
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
    };

    // Ambiguous framing is rejected, never normalized: this port is a trust
    // boundary and a proxy or future keep-alive must not disagree with us about
    // where a request ends or whose credentials it carries.
    let head = std::str::from_utf8(&buf[..header_end]).map_err(|_| ReadError::Malformed)?;
    let mut lines = head.split("\r\n");
    let mut parts = lines.next().unwrap_or("").split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(ReadError::Malformed);
    };
    if method.is_empty()
        || !target.starts_with('/')
        || target.bytes().any(|b| b.is_ascii_control())
        || !matches!(version, "HTTP/1.0" | "HTTP/1.1")
    {
        return Err(ReadError::Malformed);
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    let mut content_length = None;
    let mut bearer = None;
    let mut saw_authorization = false;
    for line in lines {
        // No obs-fold, no bare-LF lines, no whitespace around the name.
        let Some((name, value)) = line.split_once(':') else {
            return Err(ReadError::Malformed);
        };
        if name.is_empty()
            || name
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
            || line.contains('\n')
        {
            return Err(ReadError::Malformed);
        }
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(ReadError::NotImplemented);
        } else if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some()
                || value.is_empty()
                || !value.bytes().all(|b| b.is_ascii_digit())
            {
                return Err(ReadError::Malformed);
            }
            // Overflow means far over MAX_REQUEST.
            content_length = Some(value.parse().unwrap_or(usize::MAX));
        } else if name.eq_ignore_ascii_case("authorization") {
            if std::mem::replace(&mut saw_authorization, true) {
                return Err(ReadError::Malformed);
            }
            let mut p = value.splitn(2, ' ');
            if let (Some(scheme), Some(cred)) = (p.next(), p.next())
                && scheme.eq_ignore_ascii_case("bearer")
            {
                bearer = Some(cred.trim().to_owned());
            }
        }
    }
    let content_length = content_length.unwrap_or(0);
    let (method, path, query) = (method.to_owned(), path.to_owned(), query.to_owned());

    let body_start = header_end + 4;
    let body_end = body_start.saturating_add(content_length);
    if body_end > MAX_REQUEST {
        return Err(ReadError::TooLarge);
    }
    while buf.len() < body_end {
        let n = stream.read(&mut tmp).await.map_err(|_| ReadError::Closed)?;
        if n == 0 {
            break; // EOF: use what we have
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = buf[body_start.min(buf.len())..buf.len().min(body_end)].to_vec();

    Ok(Request {
        method,
        path,
        query,
        bearer,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn routes_by_method_and_path() {
        assert_eq!(route("POST", "/"), Route::Mcp);
        assert_eq!(route("POST", "/mcp"), Route::Mcp);
        assert_eq!(
            route("GET", "/"),
            Route::Respond(Response::method_not_allowed("POST"))
        );
        assert_eq!(route("GET", "/mcp"), route("GET", "/"));
        assert_eq!(
            route("POST", "/nope"),
            Route::Respond(Response::not_found())
        );
    }

    #[test]
    fn error_responses_have_no_body_and_405_has_allow() {
        let r405 = String::from_utf8(Response::method_not_allowed("POST").to_bytes()).unwrap();
        assert!(r405.starts_with("HTTP/1.1 405 "));
        assert!(r405.contains("Allow: POST\r\n"));
        assert!(r405.ends_with("\r\n\r\n"));
        assert!(Response::not_found().body.is_empty());
    }

    #[test]
    fn query_param_finds_values() {
        assert_eq!(query_param("tail=50&x=1", "tail"), Some("50"));
        assert_eq!(query_param("x=1&tail=7", "tail"), Some("7"));
        assert_eq!(query_param("flag", "flag"), Some(""));
        assert_eq!(query_param("", "tail"), None);
    }

    #[test]
    fn operator_error_serializes_code_and_hint() {
        let e = OperatorError::new(OperatorCode::PairCodeInvalid, "ask for a new code");
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"code":"PAIR_CODE_INVALID","hint":"ask for a new code"}"#
        );
    }

    #[tokio::test]
    async fn parses_request_split_across_reads() {
        let (mut client, mut server) = tokio::io::duplex(64);
        let writer = tokio::spawn(async move {
            client
                .write_all(b"POST /mcp?tail=3 HTTP/1.1\r\nauthorization: bearer tok\r\nContent-Le")
                .await
                .unwrap();
            client.write_all(b"ngth: 4\r\n\r\nab").await.unwrap();
            client.write_all(b"cd").await.unwrap();
        });
        let req = read_request(&mut server, READ_TIMEOUT).await.unwrap();
        writer.await.unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/mcp");
        assert_eq!(req.query, "tail=3");
        assert_eq!(req.bearer.as_deref(), Some("tok"));
        assert_eq!(req.body, b"abcd");
    }

    #[tokio::test]
    async fn oversize_and_malformed_requests_are_rejected() {
        let (mut c, mut s) = tokio::io::duplex(1024);
        c.write_all(b"POST / HTTP/1.1\r\nContent-Length: 999999\r\n\r\n")
            .await
            .unwrap();
        assert_eq!(
            read_request(&mut s, READ_TIMEOUT).await,
            Err(ReadError::TooLarge)
        );
        let (mut c, mut s) = tokio::io::duplex(1024);
        c.write_all(b"GARBAGE\r\n\r\n").await.unwrap();
        assert_eq!(
            read_request(&mut s, READ_TIMEOUT).await,
            Err(ReadError::Malformed)
        );
    }

    async fn read_raw(req: &[u8]) -> Result<Request, ReadError> {
        let (mut c, mut s) = tokio::io::duplex(1024);
        c.write_all(req).await.unwrap();
        read_request(&mut s, READ_TIMEOUT).await
    }

    #[tokio::test]
    async fn ambiguous_requests_are_rejected_not_normalized() {
        let bad: &[&[u8]] = &[
            // Content-Length: non-numeric, signed, empty, duplicate, conflicting.
            b"POST / HTTP/1.1\r\nContent-Length: abc\r\n\r\n",
            b"POST / HTTP/1.1\r\nContent-Length: +4\r\n\r\nabcd",
            b"POST / HTTP/1.1\r\nContent-Length:\r\n\r\n",
            b"POST / HTTP/1.1\r\nContent-Length: 4\r\ncontent-length: 4\r\n\r\nabcd",
            b"POST / HTTP/1.1\r\nContent-Length: 4\r\nContent-Length: 5\r\n\r\nabcd",
            // Duplicate Authorization.
            b"POST / HTTP/1.1\r\nAuthorization: Bearer a\r\nauthorization: Bearer b\r\n\r\n",
            // Malformed header lines.
            b"POST / HTTP/1.1\r\nno-colon\r\n\r\n",
            b"POST / HTTP/1.1\r\nContent-Length : 0\r\n\r\n",
            b"POST / HTTP/1.1\r\nX: a\r\n folded\r\n\r\n",
            // Request line: extra token, bad version, bad target.
            b"GET / HTTP/1.1 junk\r\n\r\n",
            b"GET / HTTP/9.9\r\n\r\n",
            b"GET / FOO\r\n\r\n",
            b"GET nope HTTP/1.1\r\n\r\n",
            b"GET  / HTTP/1.1\r\n\r\n",
        ];
        for req in bad {
            assert_eq!(
                read_raw(req).await,
                Err(ReadError::Malformed),
                "{}",
                String::from_utf8_lossy(req)
            );
        }
    }

    #[tokio::test]
    async fn transfer_encoding_is_not_implemented() {
        // Even alongside a valid Content-Length (the classic smuggling shape).
        for req in [
            &b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"[..],
            b"POST / HTTP/1.1\r\nContent-Length: 4\r\ntransfer-encoding: identity\r\n\r\nabcd",
        ] {
            assert_eq!(read_raw(req).await, Err(ReadError::NotImplemented));
        }
    }

    #[tokio::test]
    async fn valid_single_headers_still_parse() {
        let req = read_raw(
            b"POST /mcp HTTP/1.0\r\nAuthorization: Bearer t\r\nContent-Length: 2\r\n\r\nhi",
        )
        .await
        .unwrap();
        assert_eq!(
            (req.bearer.as_deref(), req.body.as_slice()),
            (Some("t"), &b"hi"[..])
        );
    }

    #[tokio::test(start_paused = true)]
    async fn idle_connection_times_out() {
        let (_client, mut server) = tokio::io::duplex(64);
        assert_eq!(
            read_request(&mut server, READ_TIMEOUT).await,
            Err(ReadError::Timeout)
        );
    }
}
