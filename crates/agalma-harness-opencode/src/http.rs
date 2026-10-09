//! Hand-rolled HTTP/1.1 client over `std::net::TcpStream`.
//!
//! Vendor transport lives here: the request/response wire format, Basic auth,
//! chunked decoding. No HTTP crate is used (per `docs/m0-dependencies.md`, the
//! S0d hand-rolled client is retained). Every call is a request/response pair;
//! no streaming surface is parsed here.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Reusable connection parameters for the confined server.
pub struct HttpClient {
    host: String,
    port: u16,
    /// Pre-computed `Basic <base64(user:pass)>` header value.
    auth: String,
}

impl HttpClient {
    /// Build a client for `host:port` authenticating as `user`/`password`.
    pub fn new(host: &str, port: u16, user: &str, password: &str) -> Self {
        let token = b64(format!("{user}:{password}").as_bytes());
        HttpClient {
            host: host.to_string(),
            port,
            auth: token,
        }
    }

    /// Issue an authenticated request. `body` is sent as `application/json`.
    pub fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<(u16, String), String> {
        self.send(method, path, body, true)
    }

    /// Issue an unauthenticated request (readiness probing expects `401`).
    pub fn request_unauth(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<(u16, String), String> {
        self.send(method, path, body, false)
    }

    fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
        authenticate: bool,
    ) -> Result<(u16, String), String> {
        let addr = format!("{}:{}", self.host, self.port);
        let mut stream = TcpStream::connect(&addr).map_err(|e| format!("connect {addr}: {e}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(Duration::from_secs(30)))
            .map_err(|e| e.to_string())?;

        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}:{}\r\nAccept: application/json\r\nConnection: close\r\n",
            self.host, self.port
        );
        if authenticate {
            req.push_str(&format!("Authorization: Basic {}\r\n", self.auth));
        }
        if let Some(b) = body {
            req.push_str("Content-Type: application/json\r\n");
            req.push_str(&format!("Content-Length: {}\r\n", b.len()));
        }
        req.push_str("\r\n");
        if let Some(b) = body {
            req.push_str(b);
        }
        stream
            .write_all(req.as_bytes())
            .map_err(|e| format!("write: {e}"))?;

        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .map_err(|e| format!("read: {e}"))?;
        parse_response(&raw)
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn dechunk(mut data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let line_end = find_subsequence(data, b"\r\n").ok_or("chunk size line missing")?;
        let size_line =
            std::str::from_utf8(&data[..line_end]).map_err(|_| "non-utf8 chunk size")?;
        let size_token = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_token, 16)
            .map_err(|_| format!("bad chunk size {size_token:?}"))?;
        data = &data[line_end + 2..];
        if size == 0 {
            break;
        }
        if data.len() < size {
            out.extend_from_slice(data);
            break;
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size.min(data.len())..];
        if data.len() >= 2 {
            data = &data[2..];
        } else {
            data = &[];
        }
    }
    Ok(out)
}

fn parse_response(raw: &[u8]) -> Result<(u16, String), String> {
    let sep = find_subsequence(raw, b"\r\n\r\n").ok_or("response has no header terminator")?;
    let head = std::str::from_utf8(&raw[..sep]).map_err(|_| "non-utf8 response head")?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| format!("bad status line {status_line:?}"))?;
    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    for line in lines {
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("transfer-encoding:") {
            if value.contains("chunked") {
                chunked = true;
            }
        }
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse::<usize>().ok();
        }
    }
    let body_bytes = &raw[sep + 4..];
    let body = if chunked {
        dechunk(body_bytes)?
    } else if let Some(n) = content_length {
        body_bytes[..body_bytes.len().min(n)].to_vec()
    } else {
        body_bytes.to_vec()
    };
    Ok((code, String::from_utf8_lossy(&body).into_owned()))
}

/// Standard base64 (no padding-free variants); no crate.
fn b64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"foob"), "Zm9vYg==");
        assert_eq!(b64(b"fooba"), "Zm9vYmE=");
        assert_eq!(b64(b"foobar"), "Zm9vYmFy");
        assert_eq!(b64(b"opencode:secret"), "b3BlbmNvZGU6c2VjcmV0");
    }

    #[test]
    fn parses_content_length_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 7\r\n\r\n{\"a\":1}extra";
        let (code, body) = parse_response(raw).expect("parse");
        assert_eq!(code, 200);
        assert_eq!(body, "{\"a\":1}");
    }

    #[test]
    fn parses_chunked_response() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nfoo\r\n3\r\nbar\r\n0\r\n\r\n";
        let (code, body) = parse_response(raw).expect("parse");
        assert_eq!(code, 200);
        assert_eq!(body, "foobar");
    }

    #[test]
    fn parses_401_with_empty_body() {
        let raw = b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"Secure Area\"\r\nContent-Length: 0\r\n\r\n";
        let (code, body) = parse_response(raw).expect("parse");
        assert_eq!(code, 401);
        assert!(body.is_empty());
    }
}
