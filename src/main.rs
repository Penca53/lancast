//! LanCast: cast your PC screen to smart TVs over the LAN with near-zero latency.
//! The TV only needs its built-in web browser.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod access;
mod audio;
mod clock;
mod codec;
mod config;
mod discovery;
mod engine;
mod fmp4;
mod http;
mod platform;
mod server;
#[cfg(test)]
mod smoke;
mod stream;
mod tls;
mod tvapp;
mod ui;
mod webos;
mod webrtc;
mod ws;

use anyhow::Context;
use std::{sync::atomic::Ordering, time::Duration};

const USAGE: &str = "usage: lancast [--discover] | [--export-webos-app <dir>] | [--headless [--cast <tv-ip>] [--seconds <n>] [--preset ultra|balanced|wifi]
                [--height <px>] [--fps <n>] [--kbps <n>] [--codec h264|hevc|av1] [--player auto|webrtc|webcodecs|mse] [--http|--https] [--hud] [--browser]
                [--visibility network|cast|code|pc]]";

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(windows)]
    if !args.is_empty() {
        // Release builds are GUI-subsystem apps; reuse the terminal we were launched from.
        let _ = unsafe { windows::Win32::System::Console::AttachConsole(windows::Win32::System::Console::ATTACH_PARENT_PROCESS) };
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let value = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    match args.first().map(String::as_str) {
        None => ui::run(),
        Some("--discover") => {
            for d in discovery::search(Duration::from_secs(2))? {
                println!("{:<16} {:<40} webOS={} dlna={}", d.ip, d.name, d.webos, d.av_transport.is_some());
            }
            Ok(())
        }
        Some("--export-webos-app") => {
            let dir = value("--export-webos-app").unwrap_or_else(|| "lancast-tvapp".into());
            tvapp::export(std::path::Path::new(&dir))?;
            println!("wrote {dir}; package with: ares-package {dir}");
            Ok(())
        }
        Some("--headless") => {
            let mut settings = config::Settings::load();
            let flag = |f: &str| args.iter().any(|a| a == f);
            settings.https = (settings.https || flag("--https")) && !flag("--http");
            settings.tv_stats |= flag("--hud");
            settings.tv_app &= !flag("--browser");
            match value("--preset").as_deref() {
                Some("ultra") => config::Preset::UltraLan.apply(&mut settings),
                Some("balanced") => config::Preset::Balanced.apply(&mut settings),
                Some("wifi") => config::Preset::WiFi.apply(&mut settings),
                Some(other) => anyhow::bail!("unknown preset {other:?} (ultra, balanced, wifi)"),
                None => {}
            }
            let number = |flag| value(flag).map(|v| v.parse::<u32>()).transpose();
            if let Some(h) = number("--height")? {
                settings.height = h;
            }
            if let Some(fps) = number("--fps")? {
                settings.fps = fps;
            }
            if let Some(kbps) = number("--kbps")? {
                settings.rate = config::RateControl::Cbr { kbps };
            }
            if let Some(c) = value("--codec") {
                settings.codec = config::Codec::ALL
                    .into_iter()
                    .find(|k| k.name() == c)
                    .with_context(|| format!("unknown codec {c:?} (h264, hevc, av1)"))?;
            }
            if let Some(p) = value("--player") {
                settings.player = match p.as_str() {
                    "auto" => config::Player::Auto,
                    "webrtc" => config::Player::WebRtc,
                    "webcodecs" => config::Player::WebCodecs,
                    "mse" => config::Player::Mse,
                    other => anyhow::bail!("unknown player {other:?} (auto, webrtc, webcodecs, mse)"),
                };
            }
            if let Some(v) = value("--visibility") {
                settings.visibility = config::Visibility::ALL
                    .into_iter()
                    .find(|k| k.name() == v)
                    .with_context(|| format!("unknown visibility {v:?} (network, cast, code, pc)"))?;
            }
            headless(settings, value("--cast"), value("--seconds").and_then(|s| s.parse().ok()))
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

/// Runs without a window, printing live stats: useful for scripting and smoke tests.
fn headless(settings: config::Settings, cast: Option<String>, seconds: Option<u64>) -> anyhow::Result<()> {
    let engine = engine::Engine::start(settings)?;
    println!("player: {}", engine.url_for(cast.as_deref()));
    if let Some(ip) = &cast {
        engine.cast_webos(ip, || println!("accept the prompt on your TV…"))?;
        println!("opened player on {ip}");
    }
    let s = &engine.stats;
    let (mut frames, mut bytes) = (0, 0);
    for elapsed in 1.. {
        std::thread::sleep(Duration::from_secs(1));
        let (f, b) = (s.frames.load(Ordering::Relaxed), s.video_bytes.load(Ordering::Relaxed));
        println!(
            "viewers {} | {} fps | {:.1} Mbps | peak frame {} KB | encode {:.1} ms | latency {:.1} ms | {}",
            engine.hub.viewers(),
            f - frames,
            (b - bytes) as f64 * 8.0 / 1e6,
            s.peak_frame.swap(0, Ordering::Relaxed) / 1000,
            s.encode_us.load(Ordering::Relaxed) as f64 / 1e3,
            s.latency_us.load(Ordering::Relaxed) as f64 / 1e3,
            s.status.lock().unwrap()
        );
        (frames, bytes) = (f, b);
        if seconds.is_some_and(|n| elapsed >= n) {
            break;
        }
    }
    if let Some(ip) = &cast {
        let _ = engine.stop_webos(ip);
    }
    Ok(())
}
