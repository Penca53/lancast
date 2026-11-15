//! RFC 6455 WebSocket framing, used both by the player server and the webOS control client.

use std::io::{self, Read, Write};

pub const TEXT: u8 = 0x1;
pub const BINARY: u8 = 0x2;
pub const CLOSE: u8 = 0x8;
pub const PING: u8 = 0x9;
pub const PONG: u8 = 0xA;

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// `Sec-WebSocket-Accept` value for a client key.
pub fn accept_key(key: &str) -> String {
    base64(&sha1_smol::Sha1::from(format!("{key}{GUID}")).digest().bytes())
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            s.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    s
}

/// Writes one unfragmented frame whose payload is the concatenation of `parts`
/// (avoids copying large video payloads). Clients must pass a `mask`.
pub fn write_frame(w: &mut impl Write, opcode: u8, parts: &[&[u8]], mask: Option<[u8; 4]>) -> io::Result<()> {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    let mut head = Vec::with_capacity(14);
    head.push(0x80 | opcode);
    let mask_bit = if mask.is_some() { 0x80 } else { 0 };
    match len {
        0..=125 => head.push(mask_bit | len as u8),
        126..=0xffff => {
            head.push(mask_bit | 126);
            head.extend_from_slice(&(len as u16).to_be_bytes());
        }
        _ => {
            head.push(mask_bit | 127);
            head.extend_from_slice(&(len as u64).to_be_bytes());
        }
    }
    match mask {
        None => {
            w.write_all(&head)?;
            parts.iter().try_for_each(|p| w.write_all(p))?;
        }
        Some(m) => {
            head.extend_from_slice(&m);
            let masked: Vec<u8> = parts.iter().flat_map(|p| p.iter()).enumerate().map(|(i, b)| b ^ m[i % 4]).collect();
            w.write_all(&head)?;
            w.write_all(&masked)?;
        }
    }
    w.flush()
}

/// Reads one message (reassembling fragments, unmasking client frames).
pub fn read_message(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    read_message_max(r, 64 << 20)
}

/// Like [`read_message`], refusing messages over `max` bytes (all fragments together).
pub fn read_message_max(r: &mut impl Read, max: usize) -> io::Result<(u8, Vec<u8>)> {
    let mut opcode = 0;
    let mut payload = Vec::new();
    loop {
        let mut h = [0u8; 2];
        r.read_exact(&mut h)?;
        let len = match h[1] & 0x7f {
            126 => {
                let mut b = [0; 2];
                r.read_exact(&mut b)?;
                u16::from_be_bytes(b) as u64
            }
            127 => {
                let mut b = [0; 8];
                r.read_exact(&mut b)?;
                u64::from_be_bytes(b)
            }
            n => n as u64,
        };
        if len > (max - payload.len()) as u64 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
        }
        let mask = if h[1] & 0x80 != 0 {
            let mut m = [0; 4];
            r.read_exact(&mut m)?;
            Some(m)
        } else {
            None
        };
        let start = payload.len();
        payload.resize(start + len as usize, 0);
        r.read_exact(&mut payload[start..])?;
        if let Some(m) = mask {
            payload[start..].iter_mut().enumerate().for_each(|(i, b)| *b ^= m[i % 4]);
        }
        if h[0] & 0x0f != 0 {
            opcode = h[0] & 0x0f;
        }
        if h[0] & 0x80 != 0 {
            return Ok((opcode, payload));
        }
    }
}

/// Client handshake over an already-connected stream.
pub fn client_handshake<S: Read + Write>(s: &mut S, host: &str, path: &str) -> io::Result<()> {
    let key = "bGFuY2FzdC1sYW5jYXN0IQ==";
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )?;
    s.flush()?;
    // Read the head byte-wise so no frame bytes are consumed into a buffer we would drop.
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut b = [0u8];
        s.read_exact(&mut b)?;
        head.push(b[0]);
        if head.len() > 16 * 1024 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "handshake too large"));
        }
    }
    let head = crate::http::parse_head(&String::from_utf8_lossy(&head)).unwrap_or_default();
    if head.status() != 101 || head.header("sec-websocket-accept") != Some(accept_key(key).as_str()) {
        return Err(io::Error::new(io::ErrorKind::ConnectionRefused, format!("websocket upgrade refused ({})", head.status())));
    }
    Ok(())
}

/// Server side: answers the upgrade request described by `head`.
pub fn server_handshake(w: &mut impl Write, head: &crate::http::Head) -> io::Result<()> {
    let key = head.header("sec-websocket-key").ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing key"))?;
    write!(w, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n", accept_key(key))?;
    w.flush()
}

/// True if the request head asks for a WebSocket upgrade.
pub fn is_upgrade(head: &crate::http::Head) -> bool {
    head.header("upgrade").is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// Reads the next text message of up to `max` bytes, skipping control frames; `None` on close.
pub fn read_text(r: &mut impl Read, max: usize) -> io::Result<Option<String>> {
    loop {
        match read_message_max(r, max)? {
            (TEXT, p) => return Ok(Some(String::from_utf8_lossy(&p).into_owned())),
            (CLOSE, _) => return Ok(None),
            _ => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6455_accept_key() {
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn base64_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn roundtrip_all_length_encodings_masked_and_not() {
        for len in [0usize, 5, 125, 126, 1000, 65535, 65536, 200_000] {
            for mask in [None, Some([1, 2, 3, 4])] {
                let data: Vec<u8> = (0..len).map(|i| i as u8).collect();
                let (a, b) = data.split_at(len / 3);
                let mut buf = Vec::new();
                write_frame(&mut buf, BINARY, &[a, b], mask).unwrap();
                let (op, got) = read_message(&mut buf.as_slice()).unwrap();
                assert_eq!((op, got.len()), (BINARY, len));
                assert_eq!(got, data);
            }
        }
    }

    #[test]
    fn reassembles_fragments() {
        // "Hel" (TEXT, not final) + "lo" (CONTINUATION, final)
        let buf = [0x01, 3, b'H', b'e', b'l', 0x80, 2, b'l', b'o'];
        assert_eq!(read_message(&mut &buf[..]).unwrap(), (TEXT, b"Hello".to_vec()));
    }

    #[test]
    fn refuses_messages_over_the_limit_even_when_fragmented() {
        let mut big = Vec::new();
        write_frame(&mut big, TEXT, &[&[b'x'; 100]], Some([1, 2, 3, 4])).unwrap();
        assert!(read_message_max(&mut big.as_slice(), 99).is_err());
        assert_eq!(read_message_max(&mut big.as_slice(), 100).unwrap().1.len(), 100);
        // Three 60-byte fragments: each fits, together they don't.
        let mut frag = vec![0x01, 60];
        frag.extend([b'a'; 60]);
        frag.extend([0x00, 60]);
        frag.extend([b'b'; 60]);
        frag.extend([0x80, 60]);
        frag.extend([b'c'; 60]);
        assert!(read_message_max(&mut frag.as_slice(), 150).is_err());
        assert_eq!(read_message_max(&mut frag.as_slice(), 180).unwrap().1.len(), 180);
    }

    #[test]
    fn handshake_between_client_and_server() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut r = std::io::BufReader::new(s.try_clone().unwrap());
            let head = crate::http::read_head(&mut r).unwrap();
            assert!(is_upgrade(&head));
            server_handshake(&mut &s, &head).unwrap();
            let msg = read_text(&mut r, 1024).unwrap().unwrap();
            write_frame(&mut &s, TEXT, &[msg.to_uppercase().as_bytes()], None).unwrap();
        });
        let mut c = std::net::TcpStream::connect(addr).unwrap();
        client_handshake(&mut c, &addr.to_string(), "/").unwrap();
        write_frame(&mut c, TEXT, &[b"ping"], Some([9, 8, 7, 6])).unwrap();
        assert_eq!(read_message(&mut c).unwrap(), (TEXT, b"PING".to_vec()));
        server.join().unwrap();
    }
}
