//! Minimal synchronous HTTP/HTTPS client: POST a JSON body and read one
//! response (Content-Length, chunked, or Connection:close delimited). HTTPS via
//! rustls (ring backend); plain HTTP via a bare TcpStream (for local upstreams
//! like Ollama).
//!
//! All socket operations are bounded by the caller-supplied `timeout` so a
//! slow/hung upstream surfaces as an error and the fallback chain advances
//! instead of wedging a worker thread.
//!
//! `rustls::crypto::ring::default_provider().install_default()` MUST have been
//! called once at startup before `post_json` is used over https.

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

fn tls_config() -> Arc<ClientConfig> {
    static CFG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CFG.get_or_init(|| {
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    })
    .clone()
}

/// U-24: TLS config for a backend. `None` uses the cached webpki-only config;
/// `Some(path)` trusts the webpki roots plus the CA(s) in that PEM file, so a
/// self-signed LAN upstream can be reached without a blanket insecure switch.
fn tls_config_for(ca_file: Option<&Path>) -> io::Result<Arc<ClientConfig>> {
    match ca_file {
        None => Ok(tls_config()),
        Some(path) => tls_config_with_ca(path),
    }
}

fn tls_config_with_ca(path: &Path) -> io::Result<Arc<ClientConfig>> {
    // Cache per path so repeated dispatches don't re-read/parse the PEM.
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<ClientConfig>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(cfg) = cache.lock().unwrap().get(path) {
        return Ok(cfg.clone());
    }
    let mut roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut added = 0usize;
    for cert in CertificateDer::pem_file_iter(path)
        .map_err(|e| io::Error::other(format!("ca_file {}: {e}", path.display())))?
    {
        let cert =
            cert.map_err(|e| io::Error::other(format!("ca_file {}: {e}", path.display())))?;
        roots
            .add(cert)
            .map_err(|e| io::Error::other(format!("ca_file {}: {e}", path.display())))?;
        added += 1;
    }
    if added == 0 {
        return Err(io::Error::other(format!(
            "ca_file {} contained no certificates",
            path.display()
        )));
    }
    let cfg = Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    cache
        .lock()
        .unwrap()
        .insert(path.to_path_buf(), cfg.clone());
    Ok(cfg)
}

fn request_bytes(host: &str, port: u16, path: &str, bearer: Option<&str>, body: &str) -> String {
    let auth = bearer
        .map(|b| format!("Authorization: Bearer {b}\r\n"))
        .unwrap_or_default();
    // An IPv6 literal (split_url strips its brackets) must be re-bracketed.
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    format!(
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Connect with a bounded connect timeout and apply read/write timeouts so no
/// socket operation can block indefinitely. A zero `timeout` means "no
/// read/write deadline" (connect is still clamped to a sane window).
fn connect(host: &str, port: u16, timeout: Duration) -> io::Result<TcpStream> {
    let addr = (host, port).to_socket_addrs()?.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!("cannot resolve {host}:{port}"),
        )
    })?;
    // connect_timeout rejects a zero duration; clamp to a sane window.
    let connect_to = timeout.clamp(Duration::from_secs(1), Duration::from_secs(15));
    let tcp = TcpStream::connect_timeout(&addr, connect_to)?;
    let rw = if timeout.is_zero() {
        None
    } else {
        Some(timeout)
    };
    tcp.set_read_timeout(rw)?;
    tcp.set_write_timeout(rw)?;
    Ok(tcp)
}

/// Read one HTTP response body into `buf`, bounded by `max_bytes` (S-5; 0 = no
/// cap).
///
/// F-5: when the response carries a `Content-Length`, stop reading once exactly
/// that many body bytes have arrived — a server that ignores our
/// `Connection: close` and holds the socket open would otherwise stall us until
/// the read timeout and cause a false failure / needless fallback. Chunked and
/// close-delimited responses read to EOF (unchanged).
///
/// S-5: if the body exceeds `max_bytes`, error out (bounded memory) so the chain
/// advances rather than a hostile/misbehaving upstream exhausting memory.
///
/// Tolerates an unclean close: TLS peers and some HTTP servers drop the
/// connection without a close_notify / clean FIN once the body is sent (rustls
/// 0.23 surfaces this as `UnexpectedEof`). A genuine stall instead surfaces as
/// `WouldBlock`/`TimedOut` and IS propagated.
fn read_body<R: Read>(r: &mut R, buf: &mut Vec<u8>, max_bytes: usize) -> io::Result<()> {
    let mut chunk = [0u8; 8192];
    let mut header_end: Option<usize> = None;
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    loop {
        // Saturating add: a malicious/huge Content-Length must not overflow the
        // `he + 4 + cl` framing offset (usize::MAX would wrap and truncate the
        // header terminator). On saturation the branch simply never fires and we
        // fall through to read-to-EOF, returning the (short) body intact.
        if let (Some(he), Some(cl)) = (header_end, content_length)
            && !chunked
            && let Some(end) = he.checked_add(4).and_then(|x| x.checked_add(cl))
            && buf.len() >= end
        {
            buf.truncate(end);
            return Ok(());
        }
        match r.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if max_bytes != 0 && buf.len() > max_bytes {
                    return Err(io::Error::new(
                        io::ErrorKind::FileTooLarge,
                        format!("response exceeds max_response_bytes cap ({max_bytes} bytes)"),
                    ));
                }
                if header_end.is_none()
                    && let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n")
                {
                    header_end = Some(pos);
                    let head = String::from_utf8_lossy(&buf[..pos]);
                    for line in head.lines() {
                        let l = line.to_ascii_lowercase();
                        if let Some(v) = l.strip_prefix("content-length:") {
                            content_length = v.trim().parse::<usize>().ok();
                        } else if l.starts_with("transfer-encoding:") && l.contains("chunked") {
                            chunked = true;
                        }
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}

fn parse_response(raw: &[u8]) -> io::Result<(u16, String)> {
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no header terminator"))?;
    let head = String::from_utf8_lossy(&raw[..sep]);
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad status line"))?;
    let body_bytes = &raw[sep + 4..];
    let chunked = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    let body = if chunked {
        String::from_utf8_lossy(&dechunk(body_bytes)).into_owned()
    } else {
        String::from_utf8_lossy(body_bytes).into_owned()
    };
    Ok((status, body))
}

/// Decode an HTTP/1.1 `Transfer-Encoding: chunked` body. Lenient: stops at the
/// terminating zero-size chunk or when the data is exhausted.
fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(nl) = data.windows(2).position(|w| w == b"\r\n") {
        let size_line = &data[..nl];
        let hex = size_line.split(|&b| b == b';').next().unwrap_or(size_line);
        let size = usize::from_str_radix(String::from_utf8_lossy(hex).trim(), 16).unwrap_or(0);
        data = &data[nl + 2..];
        if size == 0 {
            break;
        }
        let take = size.min(data.len());
        out.extend_from_slice(&data[..take]);
        data = &data[take..];
        if data.starts_with(b"\r\n") {
            data = &data[2..];
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
pub fn post_json(
    scheme: &str,
    host: &str,
    port: u16,
    path: &str,
    bearer: Option<&str>,
    body: &str,
    timeout: Duration,
    max_bytes: usize,
    ca_file: Option<&Path>,
) -> io::Result<(u16, String)> {
    let req = request_bytes(host, port, path, bearer, body);
    let mut raw = Vec::new();
    let tcp = connect(host, port, timeout)?;
    if scheme == "https" {
        let name = ServerName::try_from(host.to_string())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let conn =
            ClientConnection::new(tls_config_for(ca_file)?, name).map_err(io::Error::other)?;
        let mut tls = StreamOwned::new(conn, tcp);
        tls.write_all(req.as_bytes())?;
        tls.flush()?;
        read_body(&mut tls, &mut raw, max_bytes)?;
    } else {
        let mut sock = tcp;
        sock.write_all(req.as_bytes())?;
        sock.flush()?;
        read_body(&mut sock, &mut raw, max_bytes)?;
    }
    parse_response(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_request_with_bearer_and_length() {
        let r = request_bytes("h", 443, "/v1/chat/completions", Some("tok"), "{}");
        assert!(r.contains("POST /v1/chat/completions HTTP/1.1\r\n"));
        assert!(r.contains("Host: h:443\r\n"));
        assert!(r.contains("Authorization: Bearer tok\r\n"));
        assert!(r.contains("Content-Length: 2\r\n"));
        assert!(r.ends_with("\r\n\r\n{}"));
    }

    #[test]
    fn brackets_an_ipv6_literal_in_the_host_header() {
        // split_url strips the brackets from `http://[::1]:11434/v1` (U-19);
        // RFC 9112 requires them back in Host, where `::1:11434` is invalid.
        let r = request_bytes("::1", 11434, "/v1/chat/completions", None, "{}");
        assert!(r.contains("Host: [::1]:11434\r\n"), "{r}");
        let r = request_bytes("127.0.0.1", 11434, "/v1/chat/completions", None, "{}");
        assert!(r.contains("Host: 127.0.0.1:11434\r\n"), "{r}");
    }

    #[test]
    fn parses_status_and_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 7\r\n\r\n{\"ok\":1}";
        let (status, body) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "{\"ok\":1}");
    }

    #[test]
    fn dechunks_chunked_response() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let (status, body) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "hello world");
    }

    #[test]
    fn dechunk_handles_extensions_and_truncation() {
        // chunk-extension after ';' is ignored; truncated stream stops cleanly.
        assert_eq!(dechunk(b"3;foo=bar\r\nabc\r\n0\r\n\r\n"), b"abc");
        assert_eq!(dechunk(b"4\r\nab"), b"ab");
    }

    #[test]
    fn read_body_stops_at_content_length() {
        // A server that keeps the socket open past the body would stall us to a
        // read timeout; Content-Length framing stops at the declared length even
        // when trailing bytes follow.
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhelloEXTRA-BYTES-THAT-WOULD-BLOCK";
        let mut cursor: &[u8] = raw;
        let mut buf = Vec::new();
        read_body(&mut cursor, &mut buf, 0).unwrap();
        let (status, body) = parse_response(&buf).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "hello");
    }

    #[test]
    fn read_body_enforces_max_bytes() {
        // No Content-Length -> reads to EOF, but the cap stops it early (S-5).
        let mut body = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n".to_vec();
        body.resize(body.len() + 1000, b'x');
        let mut cursor: &[u8] = &body;
        let mut buf = Vec::new();
        let err = read_body(&mut cursor, &mut buf, 200).unwrap_err();
        assert!(err.to_string().contains("max_response_bytes"));
    }

    #[test]
    fn read_body_tolerates_overflowing_content_length() {
        // S-5: a hostile Content-Length near usize::MAX must not overflow the
        // `he + 4 + cl` framing offset (which would corrupt/truncate the buffer
        // or panic). The short real body is read to EOF and returned intact.
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{{\"ok\":1}}",
            u64::MAX
        );
        let mut cursor: &[u8] = raw.as_bytes();
        let mut buf = Vec::new();
        read_body(&mut cursor, &mut buf, 0).unwrap();
        let (status, body) = parse_response(&buf).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "{\"ok\":1}");
    }

    #[test]
    fn tls_config_for_none_uses_the_shared_config() {
        // U-24: no ca_file -> the cached webpki-only config, no error.
        assert!(tls_config_for(None).is_ok());
    }

    #[test]
    fn tls_config_with_ca_rejects_missing_and_certless_files() {
        // U-24: a nonexistent ca_file surfaces as an error (chain-visible).
        let missing = std::path::Path::new("/nonexistent/tmuxlet-ca.pem");
        assert!(tls_config_for(Some(missing)).is_err());

        // A readable file with no PEM certificate is rejected, not silently
        // treated as "trust nothing extra".
        let dir = std::env::temp_dir().join(format!("tmuxlet-ca-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("not-a-cert.pem");
        std::fs::write(&f, b"this is not a certificate").unwrap();
        let err = tls_config_for(Some(&f)).unwrap_err();
        assert!(
            err.to_string().contains("no certificates"),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tls_config_with_a_valid_ca_parses_and_caches() {
        // U-24 happy path: a committed, offline-generated self-signed CA PEM
        // parses, and a second call for the same path hits the cache (same Arc).
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test-ca.pem");
        let first = tls_config_for(Some(&path)).expect("a valid CA PEM must parse");
        let second = tls_config_for(Some(&path)).expect("second call must succeed");
        assert!(
            Arc::ptr_eq(&first, &second),
            "the second call must return the cached config, not re-parse"
        );
    }
}
