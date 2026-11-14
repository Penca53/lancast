//! SSDP discovery of LG webOS TVs and DLNA media renderers.

use crate::http;
use std::{
    collections::BTreeMap,
    net::{IpAddr, UdpSocket},
    time::{Duration, Instant},
};

pub const WEBOS_ST: &str = "urn:lge-com:service:webos-second-screen:1";
pub const RENDERER_ST: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
const AV_TRANSPORT: &str = "urn:schemas-upnp-org:service:AVTransport:1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub ip: String,
    pub name: String,
    /// Supports SSAP control (browser launch).
    pub webos: bool,
    /// DLNA AVTransport control URL, if the device is a media renderer.
    pub av_transport: Option<String>,
}

impl Device {
    pub fn manual(ip: &str) -> Device {
        Device { ip: ip.to_owned(), name: format!("TV at {ip}"), webos: true, av_transport: None }
    }
}

/// Local address the OS would use to reach `peer` (no packets are sent).
pub fn local_ip(peer: &str) -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect((peer, 9)).ok()?;
    Some(s.local_addr().ok()?.ip())
}

/// Sends M-SEARCH requests and gathers devices that answer within `timeout`.
pub fn search(timeout: Duration) -> std::io::Result<Vec<Device>> {
    // Bind to the LAN interface explicitly: with 0.0.0.0, Windows may route multicast through
    // a disconnected adapter that happens to have a lower metric.
    let lan = local_ip("1.1.1.1").unwrap_or(IpAddr::from([0, 0, 0, 0]));
    let sock = UdpSocket::bind((lan, 0))?;
    log::debug!("SSDP search from {}", sock.local_addr()?);
    sock.set_read_timeout(Some(Duration::from_millis(100)))?;
    for st in [WEBOS_ST, RENDERER_ST] {
        let msg = format!("M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: {st}\r\n\r\n");
        for _ in 0..2 {
            sock.send_to(msg.as_bytes(), "239.255.255.250:1900")?;
        }
    }
    let mut replies: BTreeMap<IpAddr, Vec<(String, String)>> = BTreeMap::new();
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 2048];
    while Instant::now() < deadline {
        let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
        let text = String::from_utf8_lossy(&buf[..n]);
        log::debug!("SSDP reply from {from}: {:?}", text.lines().next());
        if let Some(r) = parse_reply(&text) {
            let list = replies.entry(from.ip()).or_default();
            if !list.contains(&r) {
                list.push(r);
            }
        }
    }
    Ok(replies.into_iter().map(|(ip, rs)| describe(ip, &rs)).collect())
}

fn describe(ip: IpAddr, replies: &[(String, String)]) -> Device {
    let mut d = Device { ip: ip.to_string(), name: ip.to_string(), webos: false, av_transport: None };
    for (st, location) in replies {
        d.webos |= st == WEBOS_ST;
        let Ok((200, body)) = http::request("GET", location, &[], &[], Duration::from_secs(2)) else { continue };
        let (name, av) = parse_description(&String::from_utf8_lossy(&body), location);
        if let Some(n) = name {
            d.name = n;
        }
        d.av_transport = d.av_transport.or(av);
    }
    d
}

/// Extracts (ST, LOCATION) from an SSDP response or NOTIFY.
pub fn parse_reply(text: &str) -> Option<(String, String)> {
    let head = http::parse_head(text)?;
    let st = head.header("st").or(head.header("nt"))?;
    Some((st.to_owned(), head.header("location")?.to_owned()))
}

/// Text of the first `<tag>` element (namespace prefixes are not used by TVs here).
pub fn xml_text<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml[start..].find(&format!("</{tag}>"))?;
    Some(xml[start..end].trim())
}

/// Returns (friendlyName, absolute AVTransport control URL) from a device description.
pub fn parse_description(xml: &str, location: &str) -> (Option<String>, Option<String>) {
    let name = xml_text(xml, "friendlyName").map(unescape_xml);
    let base = xml_text(xml, "URLBase").unwrap_or(location);
    let av = xml
        .split("<service>")
        .skip(1)
        .find(|s| xml_text(s, "serviceType") == Some(AV_TRANSPORT))
        .and_then(|s| xml_text(s, "controlURL"))
        .map(|u| http::join_url(base, u));
    (name, av)
}

pub fn unescape_xml(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ssdp_reply() {
        let r = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nLOCATION: http://192.168.1.50:1407/\r\nST: urn:lge-com:service:webos-second-screen:1\r\n\r\n";
        assert_eq!(parse_reply(r), Some((WEBOS_ST.into(), "http://192.168.1.50:1407/".into())));
        assert_eq!(parse_reply("HTTP/1.1 200 OK\r\nST: x\r\n\r\n"), None);
    }

    #[test]
    fn parses_device_description() {
        let xml = r#"<?xml version="1.0"?><root><device><friendlyName>[LG] webOS TV &amp; co</friendlyName>
            <serviceList>
              <service><serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType><controlURL>rc</controlURL></service>
              <service><serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType><controlURL>/AVTransport/ctrl</controlURL></service>
            </serviceList></device></root>"#;
        let (name, av) = parse_description(xml, "http://192.168.1.50:1990/desc.xml");
        assert_eq!(name.as_deref(), Some("[LG] webOS TV & co"));
        assert_eq!(av.as_deref(), Some("http://192.168.1.50:1990/AVTransport/ctrl"));
    }

    #[test]
    fn description_without_renderer() {
        let (name, av) = parse_description("<root><friendlyName>Router</friendlyName></root>", "http://a/");
        assert_eq!((name.as_deref(), av), (Some("Router"), None));
    }

    #[test]
    fn xml_unescaping() {
        assert_eq!(unescape_xml("a&amp;b &lt;c&gt; &quot;d&quot; &amp;lt;"), r#"a&b <c> "d" &lt;"#);
    }
}
