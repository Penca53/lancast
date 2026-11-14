//! HTTP server for the player page and the WebSocket media stream.

use crate::{
    access::Access,
    clock,
    discovery::local_ip,
    fmp4, http,
    stream::{Hub, Kind, Packet},
    tls, webrtc, ws,
};
use rustls::ServerConfig;
use std::{
    io::{self, BufReader, BufWriter, Read, Write},
    net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

const PLAYER: &str = include_str!("web/player.html");
/// Big running PC clock: open it on the PC while casting and compare with the TV overlay.
const CLOCK: &str = include_str!("web/clock.html");

/// Live counters shown in the UI.
#[derive(Default)]
pub struct Stats {
    pub frames: AtomicU64,
    pub video_bytes: AtomicU64,
    /// Largest encoded frame since the last reset (bytes); shows bitrate bursts.
    pub peak_frame: AtomicU64,
    /// Smoothed capture-to-displayed-on-TV latency, as acknowledged by the player.
    pub latency_us: AtomicU64,
    /// Time from encoder input to encoded output for the latest frame.
    pub encode_us: AtomicU64,
    /// Pipeline description or last error.
    pub status: Mutex<String>,
    /// What the most recent player reported (decode path).
    pub player: Mutex<String>,
}

impl Stats {
    fn record_latency(&self, us: u64) {
        let old = self.latency_us.load(Ordering::Relaxed);
        let new = if old == 0 { us } else { (old * 7 + us) / 8 };
        self.latency_us.store(new, Ordering::Relaxed);
    }

    pub fn set_status(&self, s: impl Into<String>) {
        *self.status.lock().unwrap() = s.into();
    }
}

/// Binds `port` (falling back to any free port) and serves HTTP and HTTPS on it
/// forever, on background threads.
pub fn spawn(port: u16, tls: Arc<ServerConfig>, hub: Arc<Hub>, stats: Arc<Stats>, access: Arc<Access>) -> io::Result<u16> {
    let listener = TcpListener::bind(("0.0.0.0", port)).or_else(|_| TcpListener::bind(("0.0.0.0", 0)))?;
    let port = listener.local_addr()?.port();
    thread::Builder::new().name("http".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let (tls, hub, stats, access) = (tls.clone(), hub.clone(), stats.clone(), access.clone());
            thread::spawn(move || {
                if let Err(e) = handle(stream, tls, &hub, &stats, &access) {
                    log::debug!("connection ended: {e}");
                }
            });
        }
    })?;
    Ok(port)
}

/// The other end of a connection.
#[derive(Clone, Copy)]
struct Peer {
    addr: SocketAddr,
    /// Connected from this machine (loopback, or one of our own addresses).
    this_pc: bool,
}

/// Whether a connection from `peer` to our `local` address comes from this machine.
fn is_this_pc(peer: IpAddr, local: IpAddr) -> bool {
    peer.is_loopback() || peer == local
}

fn handle(stream: TcpStream, tls: Arc<ServerConfig>, hub: &Arc<Hub>, stats: &Arc<Stats>, access: &Access) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let addr = stream.peer_addr()?;
    // Tests connect over loopback but must exercise remote viewers.
    let peer = Peer { addr, this_pc: !cfg!(test) && is_this_pc(addr.ip(), stream.local_addr()?.ip()) };
    // Both protocols share the port: a TLS connection opens with a handshake record (0x16).
    let mut first = [0u8; 1];
    let is_tls = stream.peek(&mut first)? == 1 && first[0] == 0x16;
    let r = if is_tls {
        let (reader, writer) = tls::accept(stream.try_clone()?, tls)?;
        serve(reader, writer, peer, "https", hub, stats, access)
    } else {
        serve(stream.try_clone()?, stream.try_clone()?, peer, "http", hub, stats, access)
    };
    let _ = stream.shutdown(Shutdown::Both);
    r
}

fn serve<R: Read + Send + 'static, W: Write>(
    reader: R,
    mut w: W,
    peer: Peer,
    scheme: &str,
    hub: &Arc<Hub>,
    stats: &Arc<Stats>,
    access: &Access,
) -> io::Result<()> {
    let mut reader = BufReader::new(reader);
    let head = http::read_head(&mut reader)?;
    log::debug!("{} {scheme} {} {}", peer.addr, head.line[0], head.line[1]);
    match head.path() {
        "/ws" if ws::is_upgrade(&head) => session(reader, w, &head, peer, hub, stats, access),
        "/" | "/index.html" => http::respond(&mut w, "200 OK", "text/html; charset=utf-8", PLAYER.as_bytes()),
        "/clock" => http::respond(&mut w, "200 OK", "text/html; charset=utf-8", CLOCK.as_bytes()),
        _ => http::respond(&mut w, "404 Not Found", "text/plain", b"not found"),
    }
}

/// Streams packets to one viewer until it disconnects; the caller shuts the socket down.
fn session<R: Read + Send + 'static, W: Write>(
    mut reader: BufReader<R>,
    mut w: W,
    head: &http::Head,
    peer: Peer,
    hub: &Arc<Hub>,
    stats: &Arc<Stats>,
    access: &Access,
) -> io::Result<()> {
    ws::server_handshake(&mut w, head)?;
    let code = head.param("k");
    let allowed = || access.allows(peer.addr.ip(), peer.this_pc, code);
    let mut policy_version = access.version();
    if !allowed() {
        log::info!("refused viewer {} (not allowed by \"Who can watch\")", peer.addr);
        return deny(&mut w);
    }
    let peer = peer.addr;
    log::info!("viewer connected: {peer}");
    let mode = head.param("mode").unwrap_or_default();
    let sub = hub.subscribe();
    // WebRTC: video travels over RTP from a peer thread; the WebSocket carries signalling + audio.
    let (rtc, answers) = match mode {
        "webrtc" => {
            let (tx, rx) = mpsc::channel();
            let (answer_tx, answer_rx) = mpsc::channel();
            let ip = local_ip(&peer.ip().to_string()).unwrap_or(IpAddr::from([0, 0, 0, 0]));
            let (hub, tx2) = (hub.clone(), tx.clone());
            thread::spawn(move || {
                if let Err(e) = webrtc::run(ip, rx, tx2, answer_tx, hub) {
                    log::warn!("WebRTC session ended: {e:#}");
                }
            });
            (Some(tx), Some(answer_rx))
        }
        _ => (None, None),
    };
    let closed = Arc::new(AtomicBool::new(false));
    // Capture timestamp of the newest video packet the player has read.
    let read_pts = Arc::new(AtomicU64::new(0));
    {
        let (hub, stats, closed, rtc, read_pts) = (hub.clone(), stats.clone(), closed.clone(), rtc.clone(), read_pts.clone());
        thread::spawn(move || {
            while let Ok(Some(text)) = ws::read_text(&mut reader) {
                on_message(&text, &hub, &stats, rtc.as_ref(), &read_pts);
            }
            closed.store(true, Ordering::Relaxed);
        });
    }
    let mut flow = Flow::default();
    let mut remux = (mode == "mse").then(fmp4::Remuxer::default);
    let mut w = BufWriter::with_capacity(256 * 1024, w);
    let mut last_clock = Instant::now() - CLOCK_INTERVAL;
    let result = loop {
        // The player closed the socket (or sent a Close frame, e.g. its tab was hidden):
        // answer it to complete the closing handshake and stop streaming right away.
        if closed.load(Ordering::Relaxed) {
            let _ = ws::write_frame(&mut w, ws::CLOSE, &[], None);
            break Ok(());
        }
        // "Who can watch" changed: this viewer may no longer be allowed.
        if access.version() != policy_version {
            policy_version = access.version();
            if !allowed() {
                log::info!("viewer {peer} no longer allowed by \"Who can watch\"");
                break deny(&mut w);
            }
        }
        // PC wall clock for the player's latency check, sent outside the video pipeline.
        if last_clock.elapsed() >= CLOCK_INTERVAL {
            last_clock = Instant::now();
            if let Err(e) = send_text(&mut w, &serde_json::json!({ "t": "clock", "ms": wall_clock_ms() })) {
                break Err(e);
            }
        }
        if let Some(answers) = &answers
            && let Ok(sdp) = answers.try_recv()
            && let Err(e) = send_text(&mut w, &serde_json::json!({ "t": "answer", "sdp": sdp }))
        {
            break Err(e);
        }
        let p = match sub.rx.recv_timeout(Duration::from_millis(100)) {
            Ok(p) => p,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break Ok(()),
        };
        if p.kind == Kind::Video && rtc.is_none() && !flow.admit(&p, read_pts.load(Ordering::Relaxed), hub) {
            continue;
        }
        let r = match (&rtc, &mut remux) {
            (Some(tx), _) if p.kind == Kind::Video => {
                let _ = tx.send(webrtc::Msg::Media(p));
                Ok(())
            }
            (Some(tx), _) if p.kind == Kind::VideoConfig => {
                let _ = tx.send(webrtc::Msg::Media(p.clone()));
                send_packet(&mut w, &p)
            }
            (_, Some(m)) => m.push(&p).iter().try_for_each(|p| send_packet(&mut w, p)),
            _ => send_packet(&mut w, &p),
        };
        if let Err(e) = r {
            break Err(e);
        }
    };
    if let Some(tx) = &rtc {
        let _ = tx.send(webrtc::Msg::Close);
    }
    log::info!("viewer disconnected: {peer}");
    result
}

const CLOCK_INTERVAL: Duration = Duration::from_millis(250);

/// Most video (by capture time) allowed between what we send and what the player has read.
const MAX_IN_FLIGHT_US: u64 = 200_000;

/// Flow control over TCP: a slow player (e.g. a TV browser at a high bitrate) would otherwise
/// let video pile up unseen in socket buffers, adding seconds of delay. When too far ahead of
/// what the player has read, drop frames and resume at a fresh keyframe.
#[derive(Default)]
struct Flow {
    skipping: bool,
    /// After resuming: the keyframe's pts. Until the player reads it, the backlog is
    /// expected, so only a much larger one triggers another skip.
    resume: Option<u64>,
}

impl Flow {
    /// Whether to send video packet `p`, given the newest pts the player has read.
    fn admit(&mut self, p: &Packet, read_pts: u64, hub: &Hub) -> bool {
        if self.skipping {
            if !p.key {
                return false;
            }
            self.skipping = false;
            self.resume = Some(p.pts_us);
            return true;
        }
        let in_flight = p.pts_us.saturating_sub(read_pts);
        let limit = match self.resume {
            Some(k) if read_pts < k => MAX_IN_FLIGHT_US * 5,
            _ => {
                self.resume = None;
                MAX_IN_FLIGHT_US
            }
        };
        if read_pts > 0 && !p.key && in_flight > limit {
            log::info!("viewer {} ms behind, skipping to the next keyframe", in_flight / 1000);
            self.skipping = true;
            hub.request_key();
            return false;
        }
        true
    }
}

/// Tells the player it may not watch (it shows why instead of reconnecting) and closes.
fn deny(w: &mut impl Write) -> io::Result<()> {
    send_text(w, &serde_json::json!({ "t": "denied" }))?;
    ws::write_frame(w, ws::CLOSE, &[], None)
}

fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn send_packet(w: &mut impl Write, p: &Packet) -> io::Result<()> {
    ws::write_frame(w, ws::BINARY, &[&p.header(), &p.data], None)
}

fn send_text(w: &mut impl Write, v: &serde_json::Value) -> io::Result<()> {
    ws::write_frame(w, ws::TEXT, &[v.to_string().as_bytes()], None)
}

fn on_message(text: &str, hub: &Hub, stats: &Stats, rtc: Option<&mpsc::Sender<webrtc::Msg>>, read_pts: &AtomicU64) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else { return };
    match v["t"].as_str() {
        Some("rx") => {
            if let Some(pts) = v["pts"].as_u64() {
                read_pts.fetch_max(pts, Ordering::Relaxed);
            }
        }
        Some("ack") => {
            if let Some(pts) = v["pts"].as_u64() {
                stats.record_latency(clock::now_us().saturating_sub(pts));
            }
        }
        Some("key") => hub.request_key(),
        Some("offer") => {
            if let (Some(tx), Some(sdp)) = (rtc, v["sdp"].as_str()) {
                let _ = tx.send(webrtc::Msg::Offer(sdp.to_owned()));
            }
        }
        // WebRTC players report their receive-to-decoded delay; add our capture+encode time.
        Some("delay") => {
            if let Some(ms) = v["ms"].as_f64() {
                stats.record_latency((ms * 1000.0) as u64 + stats.encode_us.load(Ordering::Relaxed));
            }
        }
        Some("hello") => {
            let mode = v["mode"].as_str().unwrap_or("?");
            log::info!(
                "player: {mode} over {} secure={} webcodecs={} webrtc={} mse={} ({})",
                v["proto"].as_str().unwrap_or("?"),
                v["secure"],
                v["webcodecs"],
                v["webrtc"],
                v["mse"],
                v["ua"].as_str().unwrap_or("")
            );
            *stats.player.lock().unwrap() = mode.to_owned();
        }
        Some("log") => log::warn!("player: {}", v["msg"].as_str().unwrap_or("")),
        Some("stat" | "rtcstat") => log::debug!("player {v}"),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::Kind;
    use std::io::Read;

    fn start() -> (u16, Arc<Hub>, Arc<Stats>) {
        start_with(Arc::default())
    }

    /// Server whose viewers all count as remote (not this PC) under `access`.
    fn start_with(access: Arc<Access>) -> (u16, Arc<Hub>, Arc<Stats>) {
        let (hub, stats) = (Hub::new(), Arc::new(Stats::default()));
        let tls = tls::server_config(None, false, &["localhost".to_owned()]).unwrap();
        (spawn(0, tls, hub.clone(), stats.clone(), access).unwrap(), hub, stats)
    }

    fn policy(visibility: crate::config::Visibility) -> crate::access::Policy {
        let s = crate::config::Settings { visibility, access_code: "secret".into(), ..Default::default() };
        crate::access::Policy::from_settings(&s)
    }

    /// Whether the server refuses this connection (its first message says so, then it closes).
    fn refused(c: &mut TcpStream) -> bool {
        loop {
            match ws::read_message(c).unwrap() {
                (ws::TEXT, m) if m == br#"{"t":"denied"}"# => return ws::read_message(c).unwrap().0 == ws::CLOSE,
                (ws::TEXT, _) => continue, // clock ticks
                _ => return false,
            }
        }
    }

    #[test]
    fn enforces_who_can_watch_and_follows_changes() {
        use crate::config::Visibility;
        let access = Arc::new(Access::default());
        let (port, hub, _) = start_with(access.clone());
        let mut c = ws_connect(port, "/ws");
        wait_viewers(&hub, 1);
        // Narrowing the policy cuts off a viewer it no longer admits.
        access.set(policy(Visibility::ThisPc));
        assert!(refused(&mut c));
        wait_viewers(&hub, 0);
        assert!(refused(&mut ws_connect(port, "/ws")));

        access.set(policy(Visibility::Code));
        assert!(refused(&mut ws_connect(port, "/ws?mode=mse")));
        assert!(refused(&mut ws_connect(port, "/ws?mode=mse&k=guess")));
        let _ok = ws_connect(port, "/ws?mode=mse&k=secret");
        wait_viewers(&hub, 1);
        access.set(policy(Visibility::CastTargets)); // 127.0.0.1 was never cast to
        wait_viewers(&hub, 0);
    }

    #[test]
    fn this_pc_detection() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert!(is_this_pc(ip("127.0.0.1"), ip("127.0.0.1")));
        assert!(is_this_pc(ip("192.168.1.50"), ip("192.168.1.50")), "own LAN address");
        assert!(!is_this_pc(ip("192.168.1.51"), ip("192.168.1.50")));
    }

    #[test]
    fn https_and_wss_on_the_same_port() {
        let (port, hub, stats) = start();
        let connect = || tls::connect_unverified("localhost", TcpStream::connect(("127.0.0.1", port)).unwrap()).unwrap();

        let mut c = connect();
        c.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut page = Vec::new();
        let _ = c.read_to_end(&mut page); // server closes without close_notify
        assert!(String::from_utf8_lossy(&page).contains("VideoDecoder"));

        let mut c = connect();
        ws::client_handshake(&mut c, "x", "/ws").unwrap();
        wait_viewers(&hub, 1);
        hub.publish(Packet::new(Kind::Video, true, 7, vec![9; 300_000]));
        let msg = read_media(&mut c);
        assert_eq!(msg.len(), 10 + 300_000);
        let pts = clock::now_us();
        ws::write_frame(&mut c, ws::TEXT, &[format!(r#"{{"t":"ack","pts":{pts}}}"#).as_bytes()], Some([1, 2, 3, 4])).unwrap();
        eventually("ack over TLS", || stats.latency_us.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn serves_player_page_and_404() {
        let (port, ..) = start();
        let (status, body) = http::request("GET", &format!("http://127.0.0.1:{port}/"), &[], &[], Duration::from_secs(2)).unwrap();
        assert_eq!(status, 200);
        assert!(String::from_utf8_lossy(&body).contains("VideoDecoder"));
        let (status, _) = http::request("GET", &format!("http://127.0.0.1:{port}/nope"), &[], &[], Duration::from_secs(2)).unwrap();
        assert_eq!(status, 404);
    }

    fn ws_connect(port: u16, path: &str) -> TcpStream {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        ws::client_handshake(&mut s, "x", path).unwrap();
        s
    }

    #[test]
    fn flow_control_skips_to_keyframe_when_player_falls_behind() {
        let hub = Hub::new();
        let mut flow = Flow::default();
        let v = |pts: u64, key: bool| Packet::new(Kind::Video, key, pts, vec![]);
        assert!(flow.admit(&v(100_000, false), 0, &hub), "no feedback yet: send");
        assert!(flow.admit(&v(250_000, false), 100_000, &hub), "150 ms in flight: fine");
        hub.take_key_request();
        assert!(!flow.admit(&v(400_000, false), 100_000, &hub), "300 ms in flight: skip");
        assert!(hub.take_key_request(), "skipping asks for a keyframe");
        assert!(!flow.admit(&v(420_000, false), 100_000, &hub), "deltas dropped while skipping");
        assert!(flow.admit(&v(440_000, true), 100_000, &hub), "resume at keyframe");
        assert!(flow.admit(&v(700_000, false), 100_000, &hub), "grace until the keyframe is read");
        assert!(!flow.admit(&v(1_200_000, false), 100_000, &hub), "but not unbounded");
        let mut flow = Flow::default();
        assert!(flow.admit(&v(440_000, true), 100_000, &hub), "keyframes always go out");
    }

    /// Next binary (media) message, skipping text messages such as clock ticks.
    fn read_media(s: &mut impl Read) -> Vec<u8> {
        loop {
            let (op, m) = ws::read_message(s).unwrap();
            if op == ws::BINARY {
                return m;
            }
        }
    }

    #[test]
    fn sends_pc_clock_for_latency_check() {
        let (port, ..) = start();
        let mut c = ws_connect(port, "/ws");
        let (op, msg) = ws::read_message(&mut c).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&msg).unwrap();
        assert_eq!((op, v["t"].as_str()), (ws::TEXT, Some("clock")));
        assert!(v["ms"].as_u64().unwrap().abs_diff(wall_clock_ms()) < 2000);
        let (status, body) = http::request("GET", &format!("http://127.0.0.1:{port}/clock"), &[], &[], Duration::from_secs(2)).unwrap();
        assert!(status == 200 && String::from_utf8_lossy(&body).contains("latency check"));
    }

    /// Polls `cond` for up to a second (server threads process messages asynchronously).
    fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out waiting for {what}");
    }

    fn wait_viewers(hub: &Hub, n: usize) {
        eventually(&format!("{n} viewers"), || hub.viewers() == n);
    }

    #[test]
    fn streams_packets_and_handles_acks() {
        let (port, hub, stats) = start();
        let mut c = ws_connect(port, "/ws");
        wait_viewers(&hub, 1);
        assert!(hub.take_key_request());
        hub.publish(Packet::new(Kind::Video, true, 42, vec![1, 2, 3]));
        let msg = read_media(&mut c);
        assert_eq!(&msg[..10], &Packet::new(Kind::Video, true, 42, vec![]).header());
        assert_eq!(&msg[10..], &[1, 2, 3]);

        let pts = clock::now_us();
        ws::write_frame(&mut c, ws::TEXT, &[format!(r#"{{"t":"ack","pts":{pts}}}"#).as_bytes()], Some([1, 2, 3, 4])).unwrap();
        ws::write_frame(&mut c, ws::TEXT, &[br#"{"t":"key"}"#], Some([1, 2, 3, 4])).unwrap();
        eventually("latency ack", || stats.latency_us.load(Ordering::Relaxed) > 0);
        eventually("keyframe request", || hub.take_key_request());

        drop(c);
        // A disconnect is noticed via the reader thread (or the next failed write).
        hub.publish(Packet::new(Kind::Audio, false, 0, vec![0; 4]));
        wait_viewers(&hub, 0);
    }

    #[test]
    fn close_frame_ends_the_session_while_video_flows() {
        let (port, hub, _) = start();
        let mut c = ws_connect(port, "/ws");
        wait_viewers(&hub, 1);
        ws::write_frame(&mut c, ws::CLOSE, &[&1000u16.to_be_bytes()], Some([1, 2, 3, 4])).unwrap();
        // Constant-frame-rate video never leaves the session idle; it must still notice.
        let publisher = {
            let hub = hub.clone();
            thread::spawn(move || {
                for i in 0..100 {
                    hub.publish(Packet::new(Kind::Video, true, i, vec![0; 1000]));
                    thread::sleep(Duration::from_millis(10));
                }
            })
        };
        // The server answers with its own Close frame (read times out after 3 s otherwise).
        while ws::read_message(&mut c).unwrap().0 != ws::CLOSE {}
        wait_viewers(&hub, 0);
        publisher.join().unwrap();
    }

    #[test]
    fn mse_session_receives_init_segment() {
        let (port, hub, _) = start();
        let mut c = ws_connect(port, "/ws?mode=mse");
        wait_viewers(&hub, 1);
        hub.publish(Packet::new(Kind::VideoConfig, false, 0, br#"{"codec":"avc1.640033","width":640,"height":360}"#.to_vec()));
        hub.publish(Packet::new(Kind::Video, true, 1, crate::codec::tests::h264_idr()));
        let kinds: Vec<u8> = (0..3).map(|_| read_media(&mut c)[0]).collect();
        assert_eq!(kinds, [Kind::VideoConfig as u8, Kind::VideoInit as u8, Kind::Video as u8]);
    }
}
