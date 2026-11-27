//! A minimal HTTP/1.1 client over keep-alive connections, enough to time
//! responses from our own server without pulling in a client library.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

pub struct Response {
    pub status: u16,
    /// From sending the request to the first response byte.
    pub ttfb: Duration,
    pub body: Vec<u8>,
}

pub struct Conn {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl Conn {
    pub fn open(addr: SocketAddr, timeout: Duration) -> Result<Self, String> {
        let stream = TcpStream::connect_timeout(&addr, timeout).map_err(|e| e.to_string())?;
        stream
            .set_read_timeout(Some(timeout))
            .and_then(|()| stream.set_write_timeout(Some(timeout)))
            .and_then(|()| stream.set_nodelay(true))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            stream,
            buf: Vec::with_capacity(4096),
        })
    }

    pub fn get(&mut self, path: &str) -> Result<Response, String> {
        let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        self.buf.clear();
        let start = Instant::now();
        self.stream
            .write_all(request.as_bytes())
            .map_err(|e| e.to_string())?;

        let mut ttfb = None;
        let head_len = loop {
            if let Some(i) = find(&self.buf, b"\r\n\r\n") {
                break i + 4;
            }
            self.read_more()?;
            ttfb.get_or_insert_with(|| start.elapsed());
        };
        let head = std::str::from_utf8(&self.buf[..head_len]).map_err(|e| e.to_string())?;
        let (status, content_length) = parse_head(head)?;
        while self.buf.len() < head_len + content_length {
            self.read_more()?;
        }
        Ok(Response {
            status,
            ttfb: ttfb.unwrap_or_default(),
            body: self.buf[head_len..head_len + content_length].to_vec(),
        })
    }

    fn read_more(&mut self) -> Result<(), String> {
        let mut chunk = [0u8; 8192];
        match self.stream.read(&mut chunk) {
            Ok(0) => Err("connection closed".into()),
            Ok(n) => {
                self.buf.extend_from_slice(&chunk[..n]);
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    }
}

/// One request on a fresh connection; returns the status code.
pub fn status(addr: SocketAddr, path: &str, timeout: Duration) -> Result<u16, String> {
    Ok(Conn::open(addr, timeout)?.get(path)?.status)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Returns the status code and Content-Length of a response head.
fn parse_head(head: &str) -> Result<(u16, usize), String> {
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("bad status line: {status_line}"))?;
    let mut content_length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().ok();
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err("chunked responses are not supported by this client".into());
        }
    }
    Ok((
        status,
        content_length.ok_or("response has no Content-Length")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_response_head() {
        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nContent-Length: 3\r\n\r\n";
        assert_eq!(parse_head(head), Ok((200, 3)));
        assert!(parse_head("HTTP/1.1 200 OK\r\n\r\n").is_err());
        assert!(parse_head("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n").is_err());
        assert!(parse_head("garbage\r\n\r\n").is_err());
    }
}
