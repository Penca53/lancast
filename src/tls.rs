//! TLS: a persisted self-signed certificate for the player server (HTTPS unlocks WebCodecs
//! in TV browsers), and an unverified client for the TV's own self-signed control port.

use anyhow::Result;
use rustls::{
    ClientConfig, ClientConnection, DigitallySignedStruct, ServerConfig, ServerConnection, SignatureScheme, StreamOwned,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
};
use std::{
    io::{self, Read, Write},
    net::TcpStream,
    path::Path,
    sync::{Arc, Mutex},
};

/// Server config using the certificate stored in `dir`, so the browser's "proceed anyway"
/// decision keeps applying across restarts. It is (re)created when the subject names change:
/// they should include the LAN IP, so a name mismatch isn't added to the "unknown issuer" error.
pub fn server_config(dir: Option<&Path>, persist: bool, names: &[String]) -> Result<Arc<ServerConfig>> {
    let names_text = names.join("\n");
    let read = |d: &Path, f: &str| std::fs::read(d.join(f)).ok();
    let stored = dir
        .filter(|d| read(d, "cert.names").as_deref() == Some(names_text.as_bytes()))
        .and_then(|d| Some((read(d, "cert.der")?, read(d, "key.der")?)));
    let (cert, key) = match stored {
        Some(pair) => pair,
        None => {
            let ck = rcgen::generate_simple_self_signed(names.to_vec())?;
            let pair = (ck.cert.der().to_vec(), ck.signing_key.serialize_der());
            if let Some(d) = dir.filter(|_| persist) {
                let write = || -> io::Result<()> {
                    std::fs::create_dir_all(d)?;
                    std::fs::write(d.join("cert.der"), &pair.0)?;
                    std::fs::write(d.join("key.der"), &pair.1)?;
                    std::fs::write(d.join("cert.names"), &names_text)
                };
                if let Err(e) = write() {
                    log::warn!("storing TLS certificate: {e}");
                }
            }
            pair
        }
    };
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key));
    let config = ServerConfig::builder().with_no_client_auth().with_single_cert(vec![CertificateDer::from(cert)], key)?;
    Ok(Arc::new(config))
}

struct Shared {
    raw: TcpStream,
    conn: Mutex<ServerConnection>,
}

/// Read half of a server TLS stream; the TCP read happens without holding the lock,
/// so the write half can send video while we wait for the client.
pub struct Reader(Arc<Shared>);
/// Write half of a server TLS stream.
pub struct Writer(Arc<Shared>);

/// Wraps an accepted connection; the handshake runs on the first read.
pub fn accept(raw: TcpStream, config: Arc<ServerConfig>) -> io::Result<(Reader, Writer)> {
    let conn = ServerConnection::new(config).map_err(io::Error::other)?;
    let shared = Arc::new(Shared { raw, conn: Mutex::new(conn) });
    Ok((Reader(shared.clone()), Writer(shared)))
}

fn flush_tls(conn: &mut ServerConnection, mut raw: &TcpStream) -> io::Result<()> {
    while conn.wants_write() {
        conn.write_tls(&mut raw)?;
    }
    Ok(())
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut tmp = [0u8; 16 * 1024];
        loop {
            match self.0.conn.lock().unwrap().reader().read(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                r => return r,
            }
            let n = (&self.0.raw).read(&mut tmp)?;
            if n == 0 {
                return Ok(0);
            }
            let mut conn = self.0.conn.lock().unwrap();
            let mut data = &tmp[..n];
            while !data.is_empty() {
                conn.read_tls(&mut data)?;
                conn.process_new_packets().map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            }
            flush_tls(&mut conn, &self.0.raw)?;
        }
    }
}

impl Write for Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut conn = self.0.conn.lock().unwrap();
        let n = conn.writer().write(buf)?;
        flush_tls(&mut conn, &self.0.raw)?;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut conn = self.0.conn.lock().unwrap();
        conn.writer().flush()?;
        flush_tls(&mut conn, &self.0.raw)
    }
}

/// Accepts any server certificate (TVs use self-signed ones) but still checks
/// handshake signatures, so the connection is encrypted and well-formed.
#[derive(Debug)]
struct AcceptAny(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _: &CertificateDer,
        _: &[CertificateDer],
        _: &ServerName,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

pub type ClientStream = StreamOwned<ClientConnection, TcpStream>;

/// TLS client to `host` that does not verify the certificate.
pub fn connect_unverified(host: &str, tcp: TcpStream) -> Result<ClientStream> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder().dangerous().with_custom_certificate_verifier(Arc::new(AcceptAny(provider))).with_no_client_auth();
    let conn = ClientConnection::new(Arc::new(config), ServerName::try_from(host.to_owned())?)?;
    Ok(StreamOwned::new(conn, tcp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_duplex_roundtrip_and_persisted_certificate() {
        let dir = std::env::temp_dir().join(format!("lancast-tls-{}", std::process::id()));
        let names = vec!["localhost".to_owned(), "10.0.0.7".to_owned()];
        let config = server_config(Some(&dir), true, &names).unwrap();
        let cert = std::fs::read(dir.join("cert.der")).unwrap();
        server_config(Some(&dir), true, &names).unwrap();
        assert_eq!(std::fs::read(dir.join("cert.der")).unwrap(), cert, "certificate must be reused");
        server_config(Some(&dir), true, &["localhost".to_owned(), "10.0.0.8".to_owned()]).unwrap();
        assert_ne!(std::fs::read(dir.join("cert.der")).unwrap(), cert, "new IP must regenerate the certificate");
        let _ = std::fs::remove_dir_all(&dir);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let (mut r, mut w) = accept(s, config).unwrap();
            let mut buf = [0u8; 5];
            r.read_exact(&mut buf).unwrap();
            // Write from another thread while the reader half is blocked.
            let writer = std::thread::spawn(move || w.write_all(&[buf.as_slice(), &[b'!'; 100_000]].concat()).and_then(|_| w.flush()));
            let mut more = [0u8; 3];
            r.read_exact(&mut more).unwrap();
            writer.join().unwrap().unwrap();
            more
        });
        let mut c = connect_unverified("localhost", TcpStream::connect(addr).unwrap()).unwrap();
        c.write_all(b"hello").unwrap();
        let mut echo = vec![0u8; 100_005];
        c.read_exact(&mut echo).unwrap();
        assert_eq!(&echo[..5], b"hello");
        c.write_all(b"bye").unwrap();
        c.flush().unwrap();
        assert_eq!(&server.join().unwrap(), b"bye");
    }
}
