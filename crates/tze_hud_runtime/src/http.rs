//! Minimal HTTP/1.1 front end for the runtime's single HTTP port.
//!
//! One request per connection, no keep-alive, no TLS. Requests are parsed into
//! a [`Request`] with a byte body, routed by `(method, path)` through
//! [`route`], and answered with a [`Response`] carrying a byte body.
//!
//! Routes today: `POST /` and `POST /mcp` -> MCP; `GET /admin/status` and
//! `GET /admin/logs` -> operator endpoints (admin PSK only). `/pair` is
//! reserved for T6 and plugs in as a new [`Route`] variant.
//! Unknown paths get a bare 404, known paths with the wrong method a bare 405
//! with an `Allow` header (no JSON-RPC body in either case).

use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tze_hud_scene::config::AgentIdentity;

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

    pub fn text(body: impl Into<Vec<u8>>) -> Self {
        Self {
            content_type: "text/plain; charset=utf-8",
            body: body.into(),
            ..Self::empty(200)
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
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
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
    Unauthenticated,
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
    /// An operator endpoint; the caller must pass [`admin_guard`] first.
    Admin(AdminRoute),
    Respond(Response),
}

/// Operator endpoints under `/admin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminRoute {
    Status,
    Logs,
}

/// Gate for every `/admin/*` request, run before any admin data is read:
/// 401 without a valid PSK, 403 `NOT_ADMIN` for a paired agent whose allow
/// list lacks `admin` (`*` does not grant it).
pub fn admin_guard(identity: Option<&AgentIdentity>) -> Result<(), Response> {
    match identity {
        None => Err(Response::operator_error(
            401,
            &OperatorError::new(
                OperatorCode::Unauthenticated,
                "send a paired agent's PSK as the bearer token",
            ),
        )),
        Some(id) if !id.is_operator_admin() => Err(Response::operator_error(
            403,
            &OperatorError::new(
                OperatorCode::NotAdmin,
                "add \"admin\" to this agent's allow list in agents.toml",
            ),
        )),
        Some(_) => Ok(()),
    }
}

/// Route by `(method, path)`.
pub fn route(method: &str, path: &str) -> Route {
    match (method, path) {
        ("POST", "/" | "/mcp") => Route::Mcp,
        (_, "/" | "/mcp") => Route::Respond(Response::method_not_allowed("POST")),
        ("GET", "/admin/status") => Route::Admin(AdminRoute::Status),
        ("GET", "/admin/logs") => Route::Admin(AdminRoute::Logs),
        (_, "/admin/status" | "/admin/logs") => Route::Respond(Response::method_not_allowed("GET")),
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
    /// Request line was not `METHOD TARGET VERSION`.
    Malformed,
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

    let head = std::str::from_utf8(&buf[..header_end]).unwrap_or("");
    let mut lines = head.lines();
    let mut parts = lines.next().unwrap_or("").split_whitespace();
    let (Some(method), Some(target), Some(_version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(ReadError::Malformed);
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    let mut content_length = 0usize;
    let mut bearer = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().unwrap_or(0);
        } else if name.eq_ignore_ascii_case("authorization") {
            let mut p = value.splitn(2, ' ');
            if let (Some(scheme), Some(cred)) = (p.next(), p.next())
                && scheme.eq_ignore_ascii_case("bearer")
            {
                bearer = Some(cred.trim().to_owned());
            }
        }
    }
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
    fn admin_routes_are_get_only_and_guarded() {
        assert_eq!(
            route("GET", "/admin/status"),
            Route::Admin(AdminRoute::Status)
        );
        assert_eq!(route("GET", "/admin/logs"), Route::Admin(AdminRoute::Logs));
        assert_eq!(
            route("POST", "/admin/logs"),
            Route::Respond(Response::method_not_allowed("GET"))
        );
        assert_eq!(
            route("GET", "/admin/other"),
            Route::Respond(Response::not_found())
        );

        let id = |perms: &[&str]| AgentIdentity {
            agent_id: "a".into(),
            permissions: perms.iter().map(|p| p.to_string()).collect(),
        };
        assert_eq!(admin_guard(None).unwrap_err().status, 401);
        let denied = admin_guard(Some(&id(&["*"]))).unwrap_err();
        assert_eq!(denied.status, 403);
        assert!(String::from_utf8_lossy(&denied.body).contains("NOT_ADMIN"));
        assert!(admin_guard(Some(&id(&["*", "operator_admin"]))).is_ok());
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

    #[tokio::test(start_paused = true)]
    async fn idle_connection_times_out() {
        let (_client, mut server) = tokio::io::duplex(64);
        assert_eq!(
            read_request(&mut server, READ_TIMEOUT).await,
            Err(ReadError::Timeout)
        );
    }
}
