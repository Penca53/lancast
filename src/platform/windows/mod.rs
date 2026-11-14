//! Windows backend: WGC capture -> D3D11 video processor -> Media Foundation hardware encoder,
//! plus WASAPI loopback audio.

mod audio;
mod capture;
mod convert;
mod encoder;
mod gpu;

pub use audio::run_audio;
pub use gpu::monitors;

use crate::{
    clock, codec,
    config::{Codec, Settings, fit},
    server::Stats,
    stream::{Hub, Kind, Latest, Packet},
};
use anyhow::{Context, Result};
use encoder::{Encoder, Event};
use std::{
    collections::VecDeque,
    sync::{
        Once,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use windows::Win32::{
    Graphics::Direct3D11::ID3D11Texture2D,
    Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFShutdown, MFStartup},
    System::Com::{COINIT_MULTITHREADED, CoIncrementMTAUsage, CoInitializeEx},
};

pub(crate) fn com_init() {
    // Pipeline threads are the only members of the multithreaded apartment. When a pipeline
    // stops they all exit, COM tears the apartment down and unloads in-process DLLs, including
    // GraphicsCapture.dll while thread-pool work of the just-closed capture session still runs
    // in it: an access violation at an unloaded address on most pipeline stops. So keep the
    // apartment alive for the life of the process.
    static KEEP_MTA: Once = Once::new();
    KEEP_MTA.call_once(|| {
        if let Err(e) = unsafe { CoIncrementMTAUsage() } {
            log::warn!("CoIncrementMTAUsage: {e}");
        }
    });
    // Ignore "already initialized"; every thread here uses the multithreaded apartment.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

struct Frame {
    tex: ID3D11Texture2D,
    pts_us: u64,
    key: bool,
}
// SAFETY: the D3D11 device is multithread-protected; textures are only read by the encoder.
unsafe impl Send for Frame {}

struct MfSession;
impl MfSession {
    fn start() -> Result<MfSession> {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL)? };
        Ok(MfSession)
    }
}
impl Drop for MfSession {
    fn drop(&mut self) {
        let _ = unsafe { MFShutdown() };
    }
}

pub fn run_video(s: &Settings, hub: &Hub, stats: &Stats, stop: &AtomicBool) -> Result<()> {
    com_init();
    let _mf = MfSession::start()?;
    let monitors = gpu::monitor_handles();
    let (handle, monitor) = monitors.get(s.monitor).or(monitors.first()).context("no monitor found")?;
    let gpu = gpu::Gpu::for_monitor(*handle)?;
    let cap = capture::Capture::new(&gpu, *handle, s.cursor)?;
    let (w, h) = fit(cap.width, cap.height, s.height);
    let mut conv = convert::Converter::new(&gpu, (cap.width, cap.height), (w, h), s.fps)?;
    let enc = Encoder::new(&gpu, s.codec, w, h, s.fps, s.rate)?;
    stats.set_status(format!("{} | {} {}x{} -> {w}x{h} @ {} fps", enc.name, monitor.name, cap.width, cap.height, s.fps));
    log::info!("video: {}", stats.status.lock().unwrap());

    let slot = Latest::<Frame>::default();
    thread::scope(|scope| {
        let encoding = scope.spawn(|| {
            let r = encode_loop(enc, &slot, hub, stats, s.codec, (w, h, s.fps, s.rate.kbps()));
            slot.close();
            r
        });
        let r = capture_loop(&cap, &mut conv, &slot, hub, stop, s.fps);
        slot.close();
        let e = encoding.join().unwrap_or_else(|_| Err(anyhow::anyhow!("encoder thread panicked")));
        r.and(e)
    })
}

/// Feeds the encoder at a constant frame rate: the newest captured frame at each slot, or the
/// previous one again when the screen hasn't changed. Repeats encode as tiny skip frames; they
/// keep TV decoders' timelines gap-free (MSE drops everything after a gap until the next
/// keyframe) and let constant-bitrate encoding sharpen static content.
fn capture_loop(cap: &capture::Capture, conv: &mut convert::Converter, slot: &Latest<Frame>, hub: &Hub, stop: &AtomicBool, fps: u32) -> Result<()> {
    let interval = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
    let mut next = Instant::now();
    let mut last: Option<ID3D11Texture2D> = None;
    while !stop.load(Ordering::Relaxed) && !slot.is_closed() {
        thread::sleep(next.saturating_duration_since(Instant::now()));
        let fresh = match cap.newest()? {
            None if last.is_none() => cap.next(Duration::from_millis(100))?, // first frame
            f => f,
        };
        if let Some(frame) = fresh {
            last = Some(conv.convert(&capture::texture(&frame)?)?);
            let _ = frame.Close();
        }
        if let Some(tex) = &last {
            slot.put(Frame { tex: tex.clone(), pts_us: clock::now_us(), key: hub.take_key_request() });
        }
        next = (next + interval).max(Instant::now());
    }
    Ok(())
}

fn encode_loop(mut enc: Encoder, slot: &Latest<Frame>, hub: &Hub, stats: &Stats, codec: Codec, (w, h, fps, kbps): (u32, u32, u32, u32)) -> Result<()> {
    com_init();
    let mut config: Option<String> = None;
    let mut inflight: VecDeque<(u64, Instant)> = VecDeque::new();
    loop {
        match enc.next_event()? {
            Event::NeedInput => {
                let Some(f) = slot.take() else { return Ok(()) };
                enc.submit(&f.tex, f.pts_us, f.key)?;
                inflight.push_back((f.pts_us, Instant::now()));
            }
            Event::Output { data, pts_us, key } => {
                let key = key || codec::is_keyframe(codec, &data);
                if key && let Some(cs) = codec::codec_string(codec, &data).filter(|cs| config.as_ref() != Some(cs)) {
                    let json = serde_json::json!({ "codec": cs, "width": w, "height": h, "fps": fps, "kbps": kbps }).to_string();
                    hub.publish(Packet::new(Kind::VideoConfig, false, pts_us, json.into_bytes()));
                    config = Some(cs);
                }
                while let Some(&(pts, t)) = inflight.front().filter(|(pts, _)| *pts <= pts_us) {
                    inflight.pop_front();
                    if pts == pts_us {
                        stats.encode_us.store(t.elapsed().as_micros() as u64, Ordering::Relaxed);
                    }
                }
                stats.frames.fetch_add(1, Ordering::Relaxed);
                stats.video_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                stats.peak_frame.fetch_max(data.len() as u64, Ordering::Relaxed);
                hub.publish(Packet::new(Kind::Video, key, pts_us, data));
            }
            Event::Other => {}
        }
    }
}
