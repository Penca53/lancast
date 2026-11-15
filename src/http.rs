//! Just enough HTTP/1.1 for serving the player page and talking to TVs.

use std::{
    io::{self, BufRead, BufReader, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};

/// Parsed request or response head.
#[derive(Debug, Default)]
pub struct Head {
    /// Request: method, target, version. Response: version, status, reason.
    pub line: [String; 3],
    pub headers: Vec<(String, String)>,
}

impl Head {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// Request path without the query string.
    pub fn path(&self) -> &str {
        self.line[1].split('?').next().unwrap_or("")
    }

    /// Value of a query-string parameter (no percent-decoding needed for our keys).
    pub fn param(&self, key: &str) -> Option<&str> {
        let query = self.line[1].split_once('?')?.1;
        query.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == key).map(|(_, v)| v)
    }

    pub fn status(&self) -> u16 {
        self.line[1].parse().unwrap_or(0)
    }
}

/// Reads and parses a message head (up to the blank line).
pub fn read_head<R: BufRead>(r: &mut R) -> io::Result<Head> {
    let mut text = String::new();
    loop {
        let n = r.read_line(&mut text)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if text.ends_with("\r\n\r\n") || text.ends_with("\n\n") {
            break;
        }
        if text.len() > 32 * 1024 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "header too large"));
        }
    }
    parse_head(&text).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed head"))
}

pub fn parse_head(text: &str) -> Option<Head> {
    let mut lines = text.lines();
    let mut first = lines.next()?.splitn(3, ' ').map(str::to_owned);
    let line = [first.next()?, first.next()?, first.next().unwrap_or_default()];
    let headers =
        lines.take_while(|l| !l.is_empty()).filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned())).collect();
    Some(Head { line, headers })
}

pub fn respond(w: &mut impl Write, status: &str, content_type: &str, body: &[u8]) -> io::Result<()> {
    write!(
        w,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    w.write_all(body)?;
    w.flush()
}

/// Splits `http://host[:port]/path` into (`host:port`, `/path`).
pub fn split_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("http://")?;
    let (host, path) = rest.find('/').map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
    let host = if host.contains(':') { host.to_owned() } else { format!("{host}:80") };
    Some((host, path.to_owned()))
}

/// Resolves a possibly relative URL against `base` (absolute http URL). `base` comes from
/// devices on the network, so anything else must not panic: it is returned unresolved.
pub fn join_url(base: &str, url: &str) -> String {
    if url.starts_with("http://") {
        return url.to_owned();
    }
    let Some(rest) = base.strip_prefix("http://") else { return url.to_owned() };
    let origin_end = rest.find('/').map_or(base.len(), |i| i + "http://".len());
    let origin = &base[..origin_end];
    if url.starts_with('/') {
        format!("{origin}{url}")
    } else {
        let dir_end = base.rfind('/').filter(|&i| i >= origin_end).unwrap_or(origin_end);
        format!("{}/{url}", &base[..dir_end])
    }
}

/// Decodes a `Transfer-Encoding: chunked` body.
pub fn dechunk(mut data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let eol = data.windows(2).position(|w| w == b"\r\n")?;
        let size_str = std::str::from_utf8(&data[..eol]).ok()?.split(';').next()?.trim();
        let size = usize::from_str_radix(size_str, 16).ok()?;
        data = &data[eol + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(data.get(..size)?);
        data = data.get(size + 2..)?;
    }
}

/// Performs one request with `Connection: close` and returns (status, body).
pub fn request(method: &str, url: &str, headers: &[(&str, &str)], body: &[u8], timeout: Duration) -> io::Result<(u16, Vec<u8>)> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidInput, m.to_owned());
    let (host, path) = split_url(url).ok_or_else(|| bad("unsupported url"))?;
    let addr = host.to_socket_addrs()?.next().ok_or_else(|| bad("unresolvable host"))?;
    let mut s = TcpStream::connect_timeout(&addr, timeout)?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    let mut msg = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        msg += &format!("{k}: {v}\r\n");
    }
    msg += "\r\n";
    s.write_all(msg.as_bytes())?;
    s.write_all(body)?;
    let mut r = BufReader::new(s);
    let head = read_head(&mut r)?;
    let mut data = Vec::new();
    match head.header("content-length").and_then(|v| v.parse::<u64>().ok()) {
        Some(n) => r.take(n).read_to_end(&mut data)?,
        None => r.read_to_end(&mut data)?,
    };
    if head.header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        data = dechunk(&data).ok_or_else(|| bad("bad chunked body"))?;
    }
    Ok((head.status(), data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_head() {
        let mut r = io::Cursor::new(b"GET /ws?mode=mse&x=1 HTTP/1.1\r\nHost: a\r\nUpgrade: websocket\r\n\r\nBODY".to_vec());
        let h = read_head(&mut r).unwrap();
        assert_eq!(h.line[0], "GET");
        assert_eq!(h.path(), "/ws");
        assert_eq!(h.param("mode"), Some("mse"));
        assert_eq!(h.param("nope"), None);
        assert_eq!(h.header("UPGRADE"), Some("websocket"));
    }

    #[test]
    fn parses_response_status() {
        let h = parse_head("HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n").unwrap();
        assert_eq!(h.status(), 200);
        assert_eq!(h.line[2], "OK");
    }

    #[test]
    fn rejects_truncated_head() {
        let mut r = io::Cursor::new(b"GET / HTTP/1.1\r\nHost".to_vec());
        assert!(read_head(&mut r).is_err());
    }

    #[test]
    fn urls() {
        assert_eq!(split_url("http://10.0.0.5:1234/desc.xml"), Some(("10.0.0.5:1234".into(), "/desc.xml".into())));
        assert_eq!(split_url("http://tv"), Some(("tv:80".into(), "/".into())));
        assert_eq!(split_url("https://tv"), None);
        assert_eq!(join_url("http://a:1/x/desc.xml", "ctl"), "http://a:1/x/ctl");
        assert_eq!(join_url("http://a:1/x/desc.xml", "/ctl"), "http://a:1/ctl");
        assert_eq!(join_url("http://a:1", "ctl"), "http://a:1/ctl");
        assert_eq!(join_url("http://a:1/x", "http://b/y"), "http://b/y");
    }

    #[test]
    fn join_url_survives_bad_bases() {
        // A device description's <URLBase> is untrusted: short, non-http or non-ASCII text.
        for base in ["", "abc", "https://tv/", "ftp://é/x", "ééééé"] {
            assert_eq!(join_url(base, "ctl"), "ctl", "{base:?}");
        }
    }

    #[test]
    fn dechunks() {
        assert_eq!(dechunk(b"3\r\nabc\r\n2;ext\r\nde\r\n0\r\n\r\n").unwrap(), b"abcde");
        assert!(dechunk(b"5\r\nab").is_none());
    }

    #[test]
    fn request_roundtrip_against_local_server() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/hello", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let head = read_head(&mut r).unwrap();
            let mut body = vec![0; head.header("content-length").unwrap().parse().unwrap()];
            r.read_exact(&mut body).unwrap();
            respond(&mut &s, "200 OK", "text/plain", &[head.path().as_bytes(), &body].concat()).unwrap();
        });
        let (status, body) = request("POST", &url, &[("X-Test", "1")], b"!", Duration::from_secs(2)).unwrap();
        server.join().unwrap();
        assert_eq!((status, body.as_slice()), (200, b"/hello!".as_slice()));
    }
}
