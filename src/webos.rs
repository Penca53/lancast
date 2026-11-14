//! LG webOS "second screen" (SSAP) client: pairs once, then opens URLs in the TV browser.

use crate::ws;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    time::Duration,
};

pub const BROWSER: &str = "com.webos.app.browser";
const MASK: [u8; 4] = [0x6c, 0x61, 0x6e, 0x63];

trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

pub struct Session {
    s: Box<dyn Stream>,
    next_id: u32,
}

/// Registration message; with a stored `key` the TV skips the pairing prompt.
/// `signed` includes LG's test-signed manifest section, which newer firmware blacklists;
/// the unsigned variant still grants the basic permissions we need (LAUNCH).
pub fn register_message(key: Option<&str>, signed: bool) -> Value {
    let mut manifest: Value = serde_json::from_str(include_str!("webos_manifest.json")).expect("valid manifest");
    if !signed {
        let m = manifest.as_object_mut().expect("manifest object");
        m.remove("signed");
        m.remove("signatures");
    }
    let mut payload = json!({ "forcePairing": false, "pairingType": "PROMPT", "manifest": manifest });
    if let Some(k) = key {
        payload["client-key"] = k.into();
    }
    json!({ "type": "register", "id": "register_0", "payload": payload })
}

fn connect_stream(ip: &str) -> Result<Box<dyn Stream>> {
    let addr = |port| format!("{ip}:{port}").parse::<SocketAddr>();
    let tcp = |port| -> Result<TcpStream> {
        let s = TcpStream::connect_timeout(&addr(port)?, Duration::from_secs(3))?;
        // Generous read timeout: the first pairing waits for the user to accept on the TV.
        s.set_read_timeout(Some(Duration::from_secs(60)))?;
        Ok(s)
    };
    // Recent firmware only accepts TLS on 3001 (self-signed cert); older sets use plain 3000.
    let tls = || -> Result<Box<dyn Stream>> {
        let mut s = crate::tls::connect_unverified(ip, tcp(3001)?)?;
        ws::client_handshake(&mut s, &format!("{ip}:3001"), "/")?;
        Ok(Box::new(s))
    };
    tls().or_else(|e| {
        log::info!("webOS TLS connect failed ({e}), trying plain port 3000");
        let mut s = tcp(3000)?;
        ws::client_handshake(&mut s, &format!("{ip}:3000"), "/")?;
        Ok(Box::new(s))
    })
}

impl Session {
    /// Connects and registers. `on_prompt` runs if the TV is showing its pairing prompt.
    /// Returns the session and the client key to store for next time.
    pub fn connect(ip: &str, key: Option<&str>, on_prompt: impl FnOnce()) -> Result<(Session, String)> {
        let mut on_prompt = Some(on_prompt);
        match Self::register(ip, key, true, &mut on_prompt) {
            Err(e) if format!("{e:#}").contains("blacklisted") => {
                log::info!("TV rejects the signed manifest, pairing unsigned");
                Self::register(ip, key, false, &mut on_prompt)
            }
            r => r,
        }
    }

    fn register(ip: &str, key: Option<&str>, signed: bool, on_prompt: &mut Option<impl FnOnce()>) -> Result<(Session, String)> {
        let mut s = Session { s: connect_stream(ip).with_context(|| format!("connecting to TV at {ip}"))?, next_id: 0 };
        s.send(&register_message(key, signed))?;
        loop {
            let msg = s.recv()?;
            match msg["type"].as_str() {
                Some("registered") => {
                    let key = msg["payload"]["client-key"].as_str().context("no client key")?.to_owned();
                    return Ok((s, key));
                }
                Some("response") if msg["payload"]["pairingType"] == "PROMPT" => {
                    if let Some(f) = on_prompt.take() {
                        f()
                    }
                }
                Some("error") => bail!("TV refused pairing: {}", msg["error"].as_str().unwrap_or("unknown")),
                _ => {}
            }
        }
    }

    fn send(&mut self, v: &Value) -> Result<()> {
        ws::write_frame(&mut self.s, ws::TEXT, &[v.to_string().as_bytes()], Some(MASK))?;
        Ok(())
    }

    /// Next JSON message; answers keep-alive pings (the TV drops clients that don't).
    fn recv(&mut self) -> Result<Value> {
        loop {
            let (op, payload) = ws::read_message(&mut self.s)?;
            log::debug!("webOS frame op={op}: {}", String::from_utf8_lossy(&payload));
            match op {
                ws::TEXT => return Ok(serde_json::from_slice(&payload)?),
                ws::PING => ws::write_frame(&mut self.s, ws::PONG, &[&payload], Some(MASK))?,
                ws::CLOSE => bail!("TV closed the connection{}", close_reason(&payload)),
                _ => {}
            }
        }
    }

    /// Sends an SSAP request and waits for its response payload.
    pub fn request(&mut self, uri: &str, payload: Value) -> Result<Value> {
        self.next_id += 1;
        let id = format!("req_{}", self.next_id);
        self.send(&json!({ "type": "request", "id": id, "uri": uri, "payload": payload }))?;
        loop {
            let msg = self.recv()?;
            if msg["id"] != id.as_str() {
                continue;
            }
            check_response(&msg)?;
            return Ok(msg["payload"].clone());
        }
    }

    pub fn open_url(&mut self, url: &str) -> Result<()> {
        self.request("ssap://system.launcher/open", json!({ "target": url })).map(drop)
    }

    /// App currently on screen, if the TV allows reading it.
    pub fn foreground_app(&mut self) -> Option<String> {
        let p = self.request("ssap://com.webos.applicationManager/getForegroundAppInfo", json!({})).ok()?;
        p["appId"].as_str().filter(|id| !id.is_empty()).map(str::to_owned)
    }

    pub fn launch(&mut self, app: &str) -> Result<()> {
        self.launch_with(app, json!({}))
    }

    /// Launches `app` with launch parameters (fails if it isn't installed).
    pub fn launch_with(&mut self, app: &str, params: Value) -> Result<()> {
        self.request("ssap://system.launcher/launch", json!({ "id": app, "params": params })).map(drop)
    }

    /// Leaves the cast (browser or LanCast app `cast_app`): back to `previous` app if known,
    /// else close it, else go home. (Closing needs a permission that unsigned pairings don't
    /// get; launching does not.)
    pub fn leave(&mut self, cast_app: &str, previous: Option<&str>) -> Result<()> {
        if let Some(app) = previous.filter(|a| ![BROWSER, cast_app].contains(a)) {
            return self.launch(app);
        }
        self.request("ssap://system.launcher/close", json!({ "id": cast_app })).map(drop).or_else(|_| self.launch("com.webos.app.home"))
    }
}

/// Formats a WebSocket close payload (2-byte code + optional reason).
fn close_reason(payload: &[u8]) -> String {
    match payload {
        [a, b, reason @ ..] => format!(" (code {}: {})", u16::from_be_bytes([*a, *b]), String::from_utf8_lossy(reason)),
        _ => String::new(),
    }
}

fn check_response(msg: &Value) -> Result<()> {
    if msg["type"] == "error" {
        bail!("TV error: {}", msg["error"].as_str().unwrap_or("unknown"));
    }
    if msg["payload"]["returnValue"] == false {
        bail!("TV error: {}", msg["payload"]["errorText"].as_str().unwrap_or("request failed"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_message_shape() {
        let m = register_message(None, true);
        assert_eq!(m["type"], "register");
        assert!(m["payload"].get("client-key").is_none());
        assert_eq!(m["payload"]["manifest"]["signed"]["appId"], "com.lge.test");
        let perms = m["payload"]["manifest"]["permissions"].as_array().unwrap();
        assert!(perms.iter().any(|p| p == "LAUNCH"));
        assert_eq!(register_message(Some("k1"), true)["payload"]["client-key"], "k1");
    }

    #[test]
    fn unsigned_message_drops_signature_but_keeps_permissions() {
        let m = &register_message(None, false)["payload"]["manifest"];
        assert!(m.get("signed").is_none() && m.get("signatures").is_none());
        assert!(m["permissions"].as_array().unwrap().iter().any(|p| p == "LAUNCH"));
    }

    #[test]
    fn signed_section_serializes_in_sorted_key_order() {
        // The TV verifies the signature over `signed`; keys must stay in this order.
        let m = register_message(None, true);
        let s = m["payload"]["manifest"]["signed"].to_string();
        let order = ["appId", "created", "localizedAppNames", "localizedVendorNames", "permissions", "serial", "vendorId"];
        let pos: Vec<usize> = order.iter().map(|k| s.find(&format!("\"{k}\"")).unwrap()).collect();
        assert!(pos.windows(2).all(|w| w[0] < w[1]), "{s}");
    }

    #[test]
    fn response_errors() {
        assert!(check_response(&json!({"type":"response","payload":{"returnValue":true}})).is_ok());
        assert!(check_response(&json!({"type":"response","payload":{"returnValue":false,"errorText":"x"}})).is_err());
        assert!(check_response(&json!({"type":"error","error":"401 insufficient permissions"})).is_err());
    }

    /// Fake TV that, like current LG firmware, rejects the signed manifest and
    /// accepts the unsigned retry on a second connection.
    #[test]
    fn talks_to_fake_tv_with_blacklisted_signature() {
        let listener = std::net::TcpListener::bind("127.0.0.1:3000").ok();
        let Some(listener) = listener else { return }; // port busy: skip
        let tv = std::thread::spawn(move || {
            let accept = || {
                let (s, _) = listener.accept().unwrap();
                let mut r = std::io::BufReader::new(s.try_clone().unwrap());
                let head = crate::http::read_head(&mut r).unwrap();
                ws::server_handshake(&mut &s, &head).unwrap();
                let reg: Value = serde_json::from_str(&ws::read_text(&mut r).unwrap().unwrap()).unwrap();
                assert_eq!(reg["type"], "register");
                (s, r, reg)
            };
            let (s, _r, reg) = accept();
            assert!(reg["payload"]["manifest"].get("signed").is_some());
            let err = json!({"type":"error","id":"register_0","error":"403 Pairing rejected: blacklisted certificate detected","payload":{}});
            ws::write_frame(&mut &s, ws::TEXT, &[err.to_string().as_bytes()], None).unwrap();
            drop(s);

            let (s, mut r, reg) = accept();
            assert!(reg["payload"]["manifest"].get("signed").is_none());
            let reply = |v: Value| ws::write_frame(&mut &s, ws::TEXT, &[v.to_string().as_bytes()], None).unwrap();
            reply(json!({"type":"response","id":"register_0","payload":{"pairingType":"PROMPT"}}));
            // Keep-alive while the user looks at the prompt: must be answered.
            ws::write_frame(&mut &s, ws::PING, &[b"hb"], None).unwrap();
            assert_eq!(ws::read_message(&mut r).unwrap(), (ws::PONG, b"hb".to_vec()));
            reply(json!({"type":"registered","id":"register_0","payload":{"client-key":"secret"}}));
            let req: Value = serde_json::from_str(&ws::read_text(&mut r).unwrap().unwrap()).unwrap();
            assert_eq!(req["uri"], "ssap://system.launcher/open");
            assert_eq!(req["payload"]["target"], "http://pc/");
            reply(json!({"type":"response","id":"unrelated","payload":{}}));
            reply(json!({"type":"response","id":req["id"],"payload":{"returnValue":true}}));
        });
        let mut prompted = false;
        let (mut s, key) = Session::connect("127.0.0.1", None, || prompted = true).unwrap();
        assert!(prompted);
        assert_eq!(key, "secret");
        s.open_url("http://pc/").unwrap();
        tv.join().unwrap();
    }
}
