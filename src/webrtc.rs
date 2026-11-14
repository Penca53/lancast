//! WebRTC video delivery to a receive-only browser peer (ICE-lite, H.264 / H.265 / AV1 over
//! RTP/UDP).
//! TV browsers play WebRTC with a tiny jitter buffer even on plain-HTTP pages, unlike MSE.

use crate::{
    config::{self, pacing_kbps},
    stream::{Hub, Kind, Packet},
};
use anyhow::{Context, Result};
use std::{
    net::{IpAddr, SocketAddr, UdpSocket},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, Instant},
};
use str0m::{
    Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig,
    change::SdpOffer,
    format::Codec as RtpCodec,
    media::{Frequency, MediaKind, MediaTime, Mid, Pt},
    net::{Protocol, Receive},
};

pub enum Msg {
    /// Browser SDP offer.
    Offer(String),
    /// Video packet: an encoded access unit, or the stream config (sets the codec and pacing rate).
    Media(Packet),
    /// UDP datagram from the browser.
    Datagram(Vec<u8>, SocketAddr),
    /// The viewer's WebSocket session ended.
    Close,
}

/// Runs one peer connection until the viewer's session ends ([`Msg::Close`]) or ICE
/// disconnects. SDP answers are returned through `answers`.
pub fn run(local_ip: IpAddr, rx: Receiver<Msg>, tx: Sender<Msg>, answers: Sender<String>, hub: Arc<Hub>) -> Result<()> {
    let socket = UdpSocket::bind((local_ip, 0))?;
    let local = socket.local_addr()?;
    let stop = Arc::new(AtomicBool::new(false));
    forward_datagrams(socket.try_clone()?, tx, stop.clone())?;
    let result = drive(&Pacer::spawn(socket)?, local, rx, &answers, &hub);
    stop.store(true, Ordering::Relaxed);
    result
}

/// Releases datagrams at a capped rate. UDP has no congestion control, so a frame sent at
/// 10 GbE line rate arrives as one burst that overflows switch buffers and 100 Mbit TV ports;
/// spreading it over a few milliseconds avoids the loss.
struct Pacer {
    tx: Sender<(Vec<u8>, SocketAddr)>,
    /// Bytes per second.
    rate: Arc<AtomicU64>,
}

/// Unpaced burst allowance.
const BURST: Duration = Duration::from_millis(2);
/// Minimum spacing between keyframes sent in response to receiver requests.
const KEYFRAME_INTERVAL: Duration = Duration::from_millis(500);

impl Pacer {
    fn spawn(socket: UdpSocket) -> Result<Pacer> {
        let (tx, rx) = mpsc::channel::<(Vec<u8>, SocketAddr)>();
        let rate = Arc::new(AtomicU64::new(pacing_kbps(25_000) as u64 * 125));
        let r = rate.clone();
        thread::Builder::new().name("rtc-pacer".into()).spawn(move || {
            let mut next = Instant::now();
            let (mut sent, mut bytes, mut since) = (0u64, 0u64, Instant::now());
            for (data, to) in rx {
                (sent, bytes) = (sent + 1, bytes + data.len() as u64);
                if since.elapsed() >= Duration::from_secs(1) {
                    log::debug!(
                        "pacer: {sent} datagrams, {:.1} Mbit/s to {to}, rate {} Mbit/s",
                        bytes as f64 * 8e-6 / since.elapsed().as_secs_f64(),
                        r.load(Ordering::Relaxed) * 8 / 1_000_000
                    );
                    (sent, bytes, since) = (0, 0, Instant::now());
                }
                // Virtual send clock: idle time earns at most BURST of credit.
                let now = Instant::now();
                next = next.max(now.checked_sub(BURST).unwrap_or(now));
                if next > now + Duration::from_millis(1) {
                    thread::sleep(next - now); // high-resolution on Windows, unlike timed waits
                }
                if let Err(e) = socket.send_to(&data, to) {
                    log::debug!("WebRTC send: {e}");
                }
                next += Duration::from_secs_f64(data.len() as f64 / r.load(Ordering::Relaxed).max(1) as f64);
            }
        })?;
        Ok(Pacer { tx, rate })
    }

    fn send(&self, data: Vec<u8>, to: SocketAddr) {
        let _ = self.tx.send((data, to));
    }

    fn set_stream_kbps(&self, kbps: u32) {
        self.rate.store(pacing_kbps(kbps) as u64 * 125, Ordering::Relaxed);
    }
}

/// Feeds incoming UDP into the session's channel so one loop handles everything.
fn forward_datagrams(socket: UdpSocket, tx: Sender<Msg>, stop: Arc<AtomicBool>) -> Result<()> {
    socket.set_read_timeout(Some(Duration::from_millis(500)))?;
    thread::Builder::new().name("rtc-udp".into()).spawn(move || {
        let mut buf = vec![0u8; 2048];
        while !stop.load(Ordering::Relaxed) {
            if let Ok((n, from)) = socket.recv_from(&mut buf)
                && tx.send(Msg::Datagram(buf[..n].to_vec(), from)).is_err()
            {
                break;
            }
        }
    })?;
    Ok(())
}

fn drive(pacer: &Pacer, local: SocketAddr, rx: Receiver<Msg>, answers: &Sender<String>, hub: &Hub) -> Result<()> {
    let mut rtc: Option<Rtc> = None;
    let mut mid: Option<Mid> = None;
    let mut codec: Option<config::Codec> = None;
    // Payload type for the stream's codec on the negotiated video media.
    let mut video: Option<(Mid, Pt)> = None;
    // Browsers send bursts of keyframe requests while starting up; each keyframe is large.
    let mut last_key = Instant::now() - KEYFRAME_INTERVAL;
    let mut request_key = || {
        if last_key.elapsed() >= KEYFRAME_INTERVAL {
            last_key = Instant::now();
            hub.request_key();
        }
    };
    loop {
        let deadline = match &mut rtc {
            Some(rtc) => loop {
                match rtc.poll_output()? {
                    Output::Timeout(t) => break t,
                    Output::Transmit(t) => pacer.send(t.contents.into(), t.destination),
                    Output::Event(Event::IceConnectionStateChange(IceConnectionState::Disconnected)) => return Ok(()),
                    Output::Event(Event::MediaAdded(m)) if m.kind == MediaKind::Video => {
                        mid = Some(m.mid);
                        video = codec.and_then(|c| video_pt(rtc, m.mid, c)).map(|pt| (m.mid, pt));
                        log::info!("WebRTC video negotiated: {codec:?} {video:?}");
                        request_key();
                    }
                    Output::Event(Event::KeyframeRequest(r)) => {
                        log::debug!("WebRTC keyframe request: {:?}", r.kind);
                        request_key();
                    }
                    Output::Event(e) => log::debug!("WebRTC event: {e:?}"),
                }
            },
            None => Instant::now() + Duration::from_secs(1),
        };
        let now = Instant::now();
        match rx.recv_timeout(deadline.saturating_duration_since(now)) {
            Ok(Msg::Offer(sdp)) => {
                let (r, answer) = accept(&sdp, local)?;
                rtc = Some(r);
                let _ = answers.send(answer);
            }
            Ok(Msg::Media(p)) if p.kind == Kind::VideoConfig => {
                let cfg: serde_json::Value = serde_json::from_slice(&p.data).unwrap_or_default();
                if let Some(kbps) = cfg["kbps"].as_u64() {
                    pacer.set_stream_kbps(kbps as u32);
                }
                codec = cfg["codec"].as_str().and_then(config::Codec::from_codec_string);
                if let (Some(rtc), Some(m), Some(c)) = (&mut rtc, mid, codec) {
                    video = video_pt(rtc, m, c).map(|pt| (m, pt));
                    if video.is_none() {
                        log::warn!("the browser didn't offer {c:?} over WebRTC");
                    }
                }
            }
            Ok(Msg::Media(p)) => {
                if let (Some(rtc), Some((mid, pt))) = (&mut rtc, video)
                    && let Some(writer) = rtc.writer(mid)
                {
                    let rtp_time = MediaTime::new(p.pts_us * 9 / 100, Frequency::NINETY_KHZ);
                    writer.write(pt, now, rtp_time, p.data.clone())?;
                }
            }
            Ok(Msg::Datagram(data, source)) => {
                if let Some(rtc) = &mut rtc {
                    let contents = data.as_slice().try_into().context("bad datagram")?;
                    rtc.handle_input(Input::Receive(now, Receive { proto: Protocol::Udp, source, destination: local, contents }))?;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(rtc) = &mut rtc {
                    rtc.handle_input(Input::Timeout(now))?;
                }
            }
            Ok(Msg::Close) | Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

/// Builds an ICE-lite peer for the browser's offer; returns it with the SDP answer.
fn accept(offer: &str, local: SocketAddr) -> Result<(Rtc, String)> {
    let mut rtc = RtcConfig::new()
        .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
        .set_ice_lite(true)
        .clear_codecs()
        .enable_h264(true)
        .enable_h265(true)
        .enable_av1(true)
        // A large keyframe is hundreds of packets; keep them all for NACK resends.
        .set_send_buffer_video(4000)
        .build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp")?);
    let answer = rtc.sdp_api().accept_offer(SdpOffer::from_sdp_string(offer)?)?;
    Ok((rtc, answer.to_sdp_string()))
}

/// Payload type of a negotiated format for `codec` that we can send (for H.264: the
/// non-interleaved packetization our access units need).
fn video_pt(rtc: &mut Rtc, mid: Mid, codec: config::Codec) -> Option<Pt> {
    rtc.writer(mid)?
        .payload_params()
        .find(|p| {
            let spec = p.spec();
            match codec {
                config::Codec::H264 => spec.codec == RtpCodec::H264 && spec.format.packetization_mode == Some(1),
                config::Codec::Hevc => spec.codec == RtpCodec::H265,
                config::Codec::Av1 => spec.codec == RtpCodec::Av1,
            }
        })
        .map(|p| p.pt())
}
