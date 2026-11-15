//! End-to-end smoke tests against real hardware. Ignored by default; run with
//! `cargo test --release -- --ignored --nocapture`. TV tests need `LANCAST_TV=<ip>`.

use crate::{
    clock, codec,
    config::{Codec, Settings},
    engine::Engine,
    stream::Kind,
    ws,
};
use std::{
    net::TcpStream,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

struct Msg {
    kind: u8,
    key: bool,
    pts: u64,
    data: Vec<u8>,
}

fn viewer(port: u16, query: &str) -> TcpStream {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.set_nodelay(true).unwrap();
    ws::client_handshake(&mut s, "localhost", &format!("/ws{query}")).unwrap();
    s
}

fn recv(s: &mut TcpStream) -> Msg {
    let (_, m) = ws::read_message(s).expect("stream stalled");
    Msg { kind: m[0], key: m[1] & 1 == 1, pts: u64::from_le_bytes(m[2..10].try_into().unwrap()), data: m[10..].to_vec() }
}

fn send(s: &mut TcpStream, json: &str) {
    ws::write_frame(s, ws::TEXT, &[json.as_bytes()], Some([7, 7, 7, 7])).unwrap();
}

fn start(settings: Settings) -> Arc<Engine> {
    let _ = env_logger::builder().is_test(true).try_init();
    Engine::start(Settings { port: 0, ..settings }).unwrap()
}

fn run_pipeline(codec: Codec) {
    let engine = start(Settings { codec, audio: true, ..Settings::default() });
    let mut s = viewer(engine.port, "");
    let t0 = Instant::now();

    // First: video config, then an IDR keyframe whose SPS matches it.
    let (mut config, mut first_key, mut audio_cfg) = (None, None, false);
    while first_key.is_none() || t0.elapsed() < Duration::from_secs(1) {
        let m = recv(&mut s);
        match m.kind {
            k if k == Kind::VideoConfig as u8 => config = Some(String::from_utf8(m.data).unwrap()),
            k if k == Kind::AudioConfig as u8 => audio_cfg = true,
            k if k == Kind::Video as u8 && first_key.is_none() => {
                assert!(m.key, "first video packet must be a keyframe");
                assert!(codec::is_keyframe(codec, &m.data));
                first_key = Some((m.data, t0.elapsed()));
            }
            _ => {}
        }
    }
    let config: serde_json::Value = serde_json::from_str(&config.expect("no video config")).unwrap();
    let (key, startup) = first_key.unwrap();
    assert_eq!(config["codec"].as_str(), codec::codec_string(codec, &key).as_deref());
    println!("{codec:?}: config {config}, first keyframe {} KB after {startup:?}, audio config: {audio_cfg}", key.len() / 1024);

    // On-demand keyframe even when the screen is static, and capture->client latency.
    let mut lat = Vec::new();
    let mut got_key = false;
    send(&mut s, r#"{"t":"key"}"#);
    let t1 = Instant::now();
    while t1.elapsed() < Duration::from_secs(3) {
        let m = recv(&mut s);
        if m.kind == Kind::Video as u8 {
            lat.push(clock::now_us().saturating_sub(m.pts));
            send(&mut s, &format!(r#"{{"t":"ack","pts":{}}}"#, m.pts));
            got_key |= m.key;
        }
    }
    assert!(got_key, "requested keyframe never arrived");
    lat.sort_unstable();
    let st = &engine.stats;
    println!(
        "{codec:?}: {} frames in 3 s, capture->viewer latency p50 {:.2} ms / max {:.2} ms, encode {:.2} ms, status: {}",
        lat.len(),
        lat[lat.len() / 2] as f64 / 1e3,
        *lat.last().unwrap() as f64 / 1e3,
        st.encode_us.load(Ordering::Relaxed) as f64 / 1e3,
        st.status.lock().unwrap()
    );
    assert!(lat[lat.len() / 2] < 50_000, "median latency too high");
    assert!(st.latency_us() > 0, "acks not recorded");
}

#[test]
#[ignore = "needs a display and a hardware encoder"]
fn pipeline_h264() {
    run_pipeline(Codec::H264);
}

#[test]
#[ignore = "needs a display and a hardware encoder"]
fn pipeline_hevc() {
    run_pipeline(Codec::Hevc);
}

#[test]
#[ignore = "needs a display and an AV1 hardware encoder"]
fn pipeline_av1() {
    run_pipeline(Codec::Av1);
}

#[test]
#[ignore = "needs a display and a hardware encoder"]
fn pipeline_restarts_on_settings_change() {
    let engine = start(Settings::default());
    let mut s = viewer(engine.port, "");
    let size = |s: &mut TcpStream| loop {
        let m = recv(s);
        if m.kind == Kind::VideoConfig as u8 {
            let v: serde_json::Value = serde_json::from_slice(&m.data).unwrap();
            return v["height"].as_u64().unwrap();
        }
    };
    size(&mut s);
    engine.update(|s| s.height = 720);
    assert_eq!(size(&mut s), 720);
}

#[test]
#[ignore = "needs a display and a hardware encoder"]
fn pipeline_4k60_upscaled() {
    let engine = start(Settings { height: 2160, fps: 60, audio: false, ..Settings::default() });
    let mut s = viewer(engine.port, "");
    let t0 = Instant::now();
    let (mut cfg, mut frames) = (None, 0);
    while t0.elapsed() < Duration::from_secs(3) {
        let m = recv(&mut s);
        match m.kind {
            k if k == Kind::VideoConfig as u8 => cfg = Some(serde_json::from_slice::<serde_json::Value>(&m.data).unwrap()),
            k if k == Kind::Video as u8 => frames += 1,
            _ => {}
        }
    }
    let cfg = cfg.expect("no video config");
    println!("4K: {cfg}, {frames} frames in 3 s, encode {:.2} ms", engine.stats.encode_us.load(Ordering::Relaxed) as f64 / 1e3);
    assert_eq!((cfg["width"].as_u64(), cfg["height"].as_u64()), (Some(3840), Some(2160)));
    assert!(frames >= 150, "4K60 encode can't keep up: {frames} frames");
}

#[test]
#[ignore = "needs a display and H.264, HEVC and AV1 hardware encoders"]
fn mse_mode_gets_fmp4() {
    for codec in Codec::ALL {
        let engine = start(Settings { codec, audio: false, ..Settings::default() });
        let mut s = viewer(engine.port, "?mode=mse");
        let init = loop {
            let m = recv(&mut s);
            if m.kind == Kind::VideoInit as u8 {
                break m.data;
            }
        };
        assert_eq!(&init[4..8], b"ftyp");
        assert!(init.windows(4).any(|w| w == codec.fourcc().as_bytes()), "{codec:?} sample entry");
        let frag = loop {
            let m = recv(&mut s);
            if m.kind == Kind::Video as u8 {
                break m.data;
            }
        };
        assert_eq!(&frag[4..8], b"moof");
        println!("{codec:?}: init segment {} bytes, first fragment {} bytes", init.len(), frag.len());
    }
}

/// Casts to a real webOS TV and waits for its browser to decode and acknowledge frames.
#[test]
#[ignore = "needs LANCAST_TV=<ip> of a powered-on LG TV"]
fn tv_cast_webos() {
    let Ok(ip) = std::env::var("LANCAST_TV") else { panic!("set LANCAST_TV") };
    let engine = start(Settings::load());
    engine.cast_webos(&ip, || println!("accept the prompt on the TV…")).unwrap();
    let t0 = Instant::now();
    while engine.stats.latency_us() == 0 {
        assert!(t0.elapsed() < Duration::from_secs(30), "TV never displayed a frame");
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("first frame on TV after {:?}: {:?}", t0.elapsed(), engine.stats.viewers());
    for _ in 0..10 {
        std::thread::sleep(Duration::from_secs(1));
        println!(
            "TV latency (capture -> decoded/presented on TV, smoothed): {:.1} ms, frames {}",
            engine.stats.latency_us() as f64 / 1e3,
            engine.stats.frames.load(Ordering::Relaxed)
        );
    }
    engine.stop_webos(&ip).unwrap();
}
