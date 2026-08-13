//! A three-request HTTP/1.1 client, because that is all a node ever needs.
//!
//! Pairing is exactly three POSTs to a daemon we control both ends of. The
//! alternative was `reqwest`, and it was rejected for two reasons:
//!
//! 1. **Weight.** A satellite is the most resource-constrained thing in the
//!    system (docs/09 §5, low-power rule 5), and a full HTTP client — pools,
//!    redirects, cookies, decompression, its own TLS wiring — is a lot of
//!    resident surface for three requests that happen once in a node's life.
//! 2. **It hides the certificate.** The pairing check in [`crate::pairing`]
//!    needs the *bytes* of the certificate the server presented, and reqwest
//!    gives no access to them. Owning the `rustls` config is not incidental
//!    here; it is the feature.
//!
//! The scope is deliberately tiny and the limits are hard: one request per
//! connection (`Connection: close`), a bounded response, and a loud error on
//! any framing this does not implement rather than a guess.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Responses here are DTOs of a few hundred bytes. Anything larger is a
/// misconfigured peer, not a pairing response.
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// Where the daemon is, and whether we must speak TLS to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub tls: bool,
}

impl Endpoint {
    /// Parses `https://host[:port]` / `http://host[:port]`, with the trailing
    /// path (if any) discarded — the caller supplies API paths.
    pub fn parse(base_url: &str) -> Result<Self> {
        let (tls, rest) = if let Some(rest) = base_url.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = base_url.strip_prefix("http://") {
            (false, rest)
        } else {
            bail!("server URL must start with https:// or http://");
        };
        let authority = rest.split('/').next().unwrap_or(rest);
        if authority.is_empty() {
            bail!("server URL has no host");
        }
        // `[::1]:8741` — bracketed IPv6 keeps its colons.
        let (host, port) = if let Some(end) = authority.strip_prefix('[') {
            let (host, tail) = end
                .split_once(']')
                .context("IPv6 host is missing its closing bracket")?;
            let port = tail.strip_prefix(':').map(str::to_owned);
            (host.to_owned(), port)
        } else if let Some((host, port)) = authority.rsplit_once(':') {
            (host.to_owned(), Some(port.to_owned()))
        } else {
            (authority.to_owned(), None)
        };
        let port = match port {
            Some(raw) => raw.parse().context("server URL has an invalid port")?,
            None if tls => 443,
            None => 80,
        };
        Ok(Self { host, port, tls })
    }

    /// The `Host:` header value.
    fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// A parsed response: the status code and the raw body.
///
/// No `Debug`, deliberately: a pairing response body *is* the device token, and
/// the easiest way to leak a secret is to make it printable and then print it
/// in an error path (invariant 5). Tests match on the error instead.
pub struct Response {
    pub status: u16,
    pub body: String,
}

impl Response {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The RFC 9457 `detail`/`title` if the body is a problem document, else a
    /// bounded snippet — so a failure tells the owner *why* without pasting an
    /// arbitrary response into their terminal.
    pub fn problem_detail(&self) -> String {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&self.body) {
            let detail = value
                .get("detail")
                .and_then(serde_json::Value::as_str)
                .or_else(|| value.get("title").and_then(serde_json::Value::as_str));
            if let Some(detail) = detail {
                return detail.to_owned();
            }
        }
        self.body.chars().take(200).collect()
    }
}

/// POST a JSON body. `tls_config` is required when the endpoint is TLS and is
/// the caller's entire trust decision (see [`crate::pinning`]).
pub async fn post_json(
    endpoint: &Endpoint,
    tls_config: Option<Arc<rustls::ClientConfig>>,
    path: &str,
    body: &serde_json::Value,
    bearer: Option<&str>,
) -> Result<Response> {
    let body = serde_json::to_string(body).context("encoding the request body")?;
    let mut request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n",
        endpoint.authority(),
        body.len(),
    );
    if let Some(token) = bearer {
        // The token is a secret; it goes on the wire and nowhere else.
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(&body);

    let stream = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
        .await
        .with_context(|| format!("connecting to {}", endpoint.authority()))?;

    let raw = if endpoint.tls {
        let config = tls_config.context("a TLS endpoint needs a TLS configuration")?;
        let server_name = ServerName::try_from(endpoint.host.clone())
            .context("server host is not a valid TLS server name")?;
        let stream = tokio_rustls::TlsConnector::from(config)
            .connect(server_name, stream)
            .await
            .context("TLS handshake with the daemon failed")?;
        exchange(stream, request.as_bytes()).await?
    } else {
        exchange(stream, request.as_bytes()).await?
    };

    parse_response(&raw)
}

async fn exchange<S>(mut stream: S, request: &[u8]) -> Result<Vec<u8>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    stream.write_all(request).await.context("sending request")?;
    stream.flush().await.context("flushing request")?;

    // `Connection: close` means EOF terminates the response, so a bounded
    // read-to-end is both correct and the DoS ceiling.
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stream.read(&mut buffer).await.context("reading response")?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&buffer[..read]);
        if raw.len() > MAX_RESPONSE_BYTES {
            bail!("daemon response exceeded {MAX_RESPONSE_BYTES} bytes");
        }
    }
    Ok(raw)
}

fn parse_response(raw: &[u8]) -> Result<Response> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("response has no header terminator")?;
    let head = std::str::from_utf8(&raw[..split]).context("response headers are not UTF-8")?;
    let body = &raw[split + 4..];

    let mut lines = head.split("\r\n");
    let status_line = lines.next().context("response has no status line")?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .context("status line has no code")?
        .parse()
        .context("status code is not a number")?;

    // Rather than guess at framing this does not implement, say so. The daemon
    // is ours and sends Content-Length for every JSON body; if that ever
    // changes, a loud failure beats a silently truncated token.
    let mut content_length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked") {
            bail!(
                "daemon replied with chunked transfer-encoding, which this client does not parse"
            );
        }
        if name == "content-length" {
            content_length = value.parse::<usize>().ok();
        }
    }

    let body = match content_length {
        Some(len) if len <= body.len() => &body[..len],
        // Short body vs. Content-Length is a truncated response, not a partial
        // success — pairing must not proceed on one.
        Some(len) => bail!(
            "daemon response was truncated ({} of {len} bytes)",
            body.len()
        ),
        None => body,
    };

    Ok(Response {
        status,
        body: String::from_utf8_lossy(body).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_url_shapes_a_node_is_configured_with() {
        assert_eq!(
            Endpoint::parse("https://jarvis.lan:8741").expect("parse"),
            Endpoint {
                host: "jarvis.lan".into(),
                port: 8741,
                tls: true
            }
        );
        // Default ports, so an owner may omit them.
        assert_eq!(
            Endpoint::parse("https://jarvis.lan").expect("parse").port,
            443
        );
        assert_eq!(Endpoint::parse("http://127.0.0.1").expect("parse").port, 80);
        // A trailing path is not part of the authority.
        assert_eq!(
            Endpoint::parse("https://jarvis.lan:8741/api/v1")
                .expect("parse")
                .host,
            "jarvis.lan"
        );
        // IPv6 keeps its colons.
        let v6 = Endpoint::parse("http://[::1]:8741").expect("parse");
        assert_eq!((v6.host.as_str(), v6.port), ("::1", 8741));
    }

    #[test]
    fn refuses_a_url_with_no_scheme_rather_than_assuming_plaintext() {
        assert!(Endpoint::parse("jarvis.lan:8741").is_err());
        assert!(Endpoint::parse("ws://jarvis.lan:8741").is_err());
    }

    #[test]
    fn parses_a_response_with_content_length() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 9\r\n\r\n{\"a\": 1}\n";
        let response = parse_response(raw).expect("parse");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "{\"a\": 1}\n");
        assert!(response.is_success());
    }

    /// `Response` has no `Debug`, so unwrap the error by hand rather than
    /// giving a token-bearing type a printable impl just to satisfy a test.
    fn expect_refusal(raw: &[u8]) -> anyhow::Error {
        match parse_response(raw) {
            Ok(_) => panic!("must refuse"),
            Err(e) => e,
        }
    }

    #[test]
    fn a_truncated_body_is_an_error_not_a_partial_success() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 500\r\n\r\n{\"deviceToken\": \"trunc";
        assert!(expect_refusal(raw).to_string().contains("truncated"));
    }

    #[test]
    fn chunked_framing_fails_loudly_rather_than_being_misread() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let error = expect_refusal(raw);
        assert!(error.to_string().contains("chunked"), "{error}");
    }

    #[test]
    fn surfaces_the_problem_detail_from_an_rfc_9457_body() {
        let response = Response {
            status: 403,
            body: r#"{"title":"pairing failed","detail":"no open pairing window"}"#.into(),
        };
        assert!(!response.is_success());
        assert_eq!(response.problem_detail(), "no open pairing window");
    }
}
