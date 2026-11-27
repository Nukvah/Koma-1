//! The client side of the health check, used by `koma healthcheck` (the
//! image's `HEALTHCHECK`; a `FROM scratch` image has no curl).

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::Duration;

/// Where to probe a server that listens on `listen`: a wildcard address
/// (0.0.0.0 or ::) is reached through loopback.
pub fn probe_target(listen: SocketAddr) -> SocketAddr {
    let ip = match listen.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, listen.port())
}

/// `Ok` if `GET /healthz` answers 200.
pub fn probe(addr: SocketAddr, timeout: Duration) -> Result<(), String> {
    match status(addr, "/healthz", timeout)? {
        200 => Ok(()),
        code => Err(format!("status {code}")),
    }
}

/// Sends `GET path` and returns the response status code.
pub fn status(addr: SocketAddr, path: &str, timeout: Duration) -> Result<u16, String> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(timeout)))
        .map_err(|e| e.to_string())?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|e| e.to_string())?;

    // The status line is all we need: "HTTP/1.1 200 OK".
    let mut head = [0u8; 64];
    let mut len = 0;
    while len < 12 {
        match stream.read(&mut head[len..]) {
            Ok(0) => break,
            Ok(n) => len += n,
            Err(e) => return Err(e.to_string()),
        }
    }
    parse_status(&head[..len]).ok_or_else(|| "not an HTTP response".to_owned())
}

fn parse_status(head: &[u8]) -> Option<u16> {
    let rest = head.strip_prefix(b"HTTP/1.")?.get(2..5)?;
    std::str::from_utf8(rest).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_is_probed_on_loopback() {
        let v4: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        assert_eq!(probe_target(v4), "127.0.0.1:8080".parse().unwrap());
        let v6: SocketAddr = "[::]:81".parse().unwrap();
        assert_eq!(probe_target(v6), "[::1]:81".parse().unwrap());
        let other: SocketAddr = "10.0.0.2:80".parse().unwrap();
        assert_eq!(probe_target(other), other);
    }

    #[test]
    fn status_line() {
        assert_eq!(parse_status(b"HTTP/1.1 200 OK\r\n"), Some(200));
        assert_eq!(parse_status(b"HTTP/1.0 404 Not Found"), Some(404));
        assert_eq!(parse_status(b"SSH-2.0-x"), None);
        assert_eq!(parse_status(b"HTTP/1.1 2"), None);
    }
}
