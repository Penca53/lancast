# LanCast

[![CI](https://github.com/Penca53/lancast/actions/workflows/ci.yml/badge.svg)](https://github.com/Penca53/lancast/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Penca53/lancast?label=release)](https://github.com/Penca53/lancast/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/Penca53/lancast/total)](https://github.com/Penca53/lancast/releases)
[![Platform](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-4f46e5)](https://penca53.github.io/lancast/)
[![License](https://img.shields.io/github/license/Penca53/lancast)](LICENSE)
[![Website](https://img.shields.io/badge/website-penca53.github.io%2Flancast-059669)](https://penca53.github.io/lancast/)

Cast your whole PC screen (with audio) to a smart TV or any browser over the LAN, up to 4K60.
Latency depends on what the receiving browser allows: ~20–60 ms where WebRTC or WebCodecs can
display, ~0.12–0.26 s to playback on LG webOS 25 TVs (see below).
**Nothing to install on the TV:** it plays in the TV's built-in web browser, and on LG webOS
LanCast opens that browser for you.

**[Download for Windows](https://penca53.github.io/lancast/)**: website with a one-click download and a plain-language guide.

> **Status:** Windows only for now (Linux and macOS are on the roadmap). Tested only on LG webOS TVs;
> other TVs and browsers should work if their browser can play the stream.

## Use it

1. Run `lancast.exe`. Your TV shows up under **TVs** (it must be on and on the same network).
2. Click **Cast**. The first time, the TV asks you to allow the connection; LanCast then
   remembers it. **Stop** returns the TV to whatever it was showing before.
3. Any other device can watch too: open the link under **Link** in its browser.

**Who can watch** (under **Link**) decides which devices may open the stream; this PC always can:

| Setting | Who |
|---|---|
| Anyone on the network (default) | Any device that opens the link |
| Only TVs I cast to | TVs you've pressed **Cast** for, remembered by IP (**Forget TVs** clears them) |
| Anyone with the link | Devices whose link carries the secret code; casting adds it, **New code** revokes old links |
| This PC only | Browsers on this PC |

Changing it cuts off players it no longer admits; they show why and check again every 10 s.
While someone watches, **Live** lists each viewer with its player and latency.

On the TV remote: **OK** turns on sound and full screen (browsers block sound until a key is pressed).
**Red** or **Info** shows live stats.

If the PC can't stream (for example, no hardware encoder for the chosen codec), the app says why
and so does the TV. Codecs the PC's graphics can't encode are greyed out.

CLI, handy for scripts and testing:

```
lancast --discover                                    # list TVs on the network
lancast --headless                                    # serve only, print live stats every second
lancast --headless --cast 192.168.1.50 --seconds 30   # cast to a webOS TV for 30 s
        [--preset ultra|balanced|wifi] [--height 2160] [--fps 60] [--kbps 40000]
        [--codec h264|hevc|av1] [--player auto|webrtc|webcodecs|mse] [--http|--https]
        [--visibility network|cast|code|pc]
        [--hud]                                       # stats overlay on the TV
        [--browser]                                   # cast to the TV's browser even if the TV app is installed
lancast --export-webos-app lancast-tvapp              # write the webOS TV app (see below)
```

### LanCast TV app (optional, LG webOS)

The same player, packaged to run from the TV instead of being served to its browser. A page
running from the TV counts as a secure context, which browsers require for WebCodecs.
`lancast --export-webos-app <dir>` writes the app; package it with `ares-package <dir>` and
install it with `ares-install` (the TV must be in developer mode). With **Use TV app** on (the
default), Cast opens the app when it's installed and the browser otherwise.

## How it works

```
 Windows.Graphics.Capture ─► D3D11 video processor ─► Media Foundation HW encoder ─┬─► WebRTC (RTP/UDP, paced) ─► TV browser <video>
   (GPU texture, cursor,      (BGRA→NV12 BT.709,       (NVENC / AMF / QSV, low-     ├─► WebSocket: WebCodecs (secure pages only)
    constant frame rate)       scaling, on GPU)          latency, no B-frames,       └─► WebSocket: fMP4 for MSE (fallback)
                                                          on-demand IDR)
 WASAPI loopback ──────────────────────────────────────► s16 PCM over WebSocket ─────────────────────────► WebAudio, synced to the video
```

- Frames never leave the GPU until they're compressed.
- The encoder gets a **constant frame rate**: an unchanged screen is re-sent as tiny "skip" frames,
  which keeps TV decoders' timelines gap-free.
- Keyframes are sent **on demand** (new viewer, loss, and every ~3 s for MSE players, which can
  only trim their buffers at one), so there's no fixed keyframe interval. WebRTC receivers' requests
  are limited to one every 500 ms.
- **Paced UDP:** WebRTC packets are released at a capped rate (≤95 Mbit/s unless the stream needs more),
  so a frame doesn't hit the network as one 10 GbE burst that overflows a 100 Mbit TV port.
- The player reports back what it decoded/displayed, so **"ms to screen"** is measured, not guessed.

### Player paths (chosen automatically)

| Player | Needs | Latency |
|---|---|---|
| WebCodecs | Secure page (HTTPS or localhost) | 4–6 ms in a desktop browser |
| WebRTC | Any modern browser, plain HTTP; **not LG webOS** | ~10–25 ms in desktop Chrome |
| MSE | Any browser | ~0.12–0.26 s on an LG webOS TV (held just ahead of the TV's stall point) |

On **LG webOS 25** only MSE can show a picture:

- WebCodecs needs a secure page, and the browser's certificate-warning "Proceed" never loads the
  page (no request reaches the PC), whatever the self-signed certificate contains.
- WebRTC connects and decodes at 60 fps with ~25 ms delay, but the frames go to a hidden
  hardware video layer: the screen stays black, and canvas copies come out black too.
- MSE plays at up to **4K60** with no dropped frames. The player keeps only 0.12 s (25 Mbps) to
  0.25 s (40 Mbps) of video buffered: less and the TV stalls. A stall, seek or (re)start makes the
  TV refill ~1 s before playing, and LanCast catches up at 1.1x speed over ~10 s; seeking
  ahead instead just triggers another refill. Each speed change risks a stall too, so the player
  changes speed rarely (hysteresis), never above 1.1x.
  It buffers by time, not by frame count, so 120 fps doesn't help (and drops ~5% of frames).

Auto therefore picks MSE on webOS (detected from the browser's user agent).

### Measured

RTX 5080, 2560×1440 at 60 fps:

| | |
|---|---|
| Capture → encoded (NVENC H.264) | 2–5 ms |
| Capture → encoded, upscaled to 3840×2160 (4K60) | ~7 ms |
| Capture → drawn in a local browser (WebCodecs) | 4–6 ms |
| LG webOS TV, MSE 1440p60, capture → TV playback position, 25 Mbps | ~120–135 ms, 0 stalls in 5 min |
| LG webOS TV, MSE 1440p60, capture → TV playback position, 40 Mbps | ~260 ms, 0 stalls in 3 min (50+ Mbps stalls) |
| LG webOS TV, 1440p60 at 25 Mbps, screen to screen, old 0.6 s buffer (by eye) | ~0.9–1 s |
| LG webOS TV, WebRTC (decoded but not displayed) | ~25 ms, 0 packet loss with pacing |

The TV's panel adds its own processing on top; use the TV's Game mode.

### Settings

| Preset | Resolution | FPS | Bitrate |
|---|---|---|---|
| Ultra (wired LAN) | native | 60 | 25 Mbps CBR |
| Balanced | 1080p | 60 | 15 Mbps CBR |
| Wi-Fi | 1080p | 30 | 10 Mbps CBR |

Ultra is the lowest-latency clean setting on an LG webOS TV (browser player): ~0.12 s of buffer at
25 Mbps. 40 Mbps (under **Advanced**) looks better and plays cleanly with ~0.25 s of buffer,
which the player picks automatically; 50+ Mbps stalls.

### Latency check

Turn on **Stats on TV** and click **Latency check** in the app. A big clock opens on the PC.
The TV then shows the PC's live clock (sent outside the video) in its top-left corner. The
difference from the clock in the video is the real screen-to-screen delay.

**A/V sync:** on the TV, audio is scheduled to match the delayed video (both carry PC capture
timestamps). The TV's own picture and sound processing add delays the page can't see, so click
**Start A/V sync test** on the clock page: it flashes and beeps every second. If the beep and the
flash don't coincide on the TV, press **left/right** on the remote to shift the audio by 20 ms
(the TV remembers it).

### Keeping the delay bounded

- **Flow control:** the player reports how far it has read; when more than 200 ms of video is in
  flight, LanCast drops frames and resumes at a fresh keyframe, instead of letting a slow
  receiver build up seconds of hidden TCP backlog.
- **Small TV buffers:** the MSE player asks for a keyframe every ~3 s and trims old video at
  each one; TV media buffers are small, and overflowing them stalls playback.
- **No bitrate bursts:** the encoder's rate-control buffer is two frames, as game streamers use.
  NVENC's default allowed ~1.5x bursts for a second (37 Mbps peaks at 25 Mbps), enough to
  starve the TV's small playback buffer.

**Advanced** options: monitor, output resolution (native, 2160p/4K, 1440p, 1080p, 720p; scales up or
down on the GPU), 30/60/120 fps, codec, constant bitrate or constant quality, player (Auto/WebCodecs/WebRTC/MSE), audio,
cursor, TV app, stats on TV, HTTPS. Settings and the TLS certificate live in `%APPDATA%\lancast\`.

**Codecs** (8-bit 4:2:0, hardware-encoded, all three players):

| Codec | PC encoder | Notes |
|---|---|---|
| H.264 (default) | any NVENC / AMF / QSV | plays everywhere |
| HEVC | NVENC / AMF / QSV (2016+ GPUs) | visibly sharper at the same bitrate on an LG webOS TV |
| AV1 | RTX 40+, Radeon RX 7000+, Intel Arc | LG webOS plays it over MSE (2560x1440@60, ~150 ms, same as H.264) |

If the TV's browser can't decode the chosen codec, the WebCodecs and WebRTC players fall back to
MSE, and MSE shows "This TV can't play … video" instead of a black screen.

## LG webOS notes

- Current firmware rejects the signed pairing manifest that third-party remotes use
  ("blacklisted certificate"); LanCast falls back to unsigned pairing, which still allows opening apps.
  Unsigned pairing can't close the browser, so Stop switches back to the previous app.
- `src/webos_manifest.json` is that signed manifest, as published in LG's open-source
  [Connect SDK](https://github.com/ConnectSDK) (Apache-2.0) and shipped by
  other webOS remote tools. LanCast still offers it first, in case a TV accepts it.
- The browser keeps background tabs alive with their decoders, and stale tabs can block new
  WebRTC sessions. The player therefore releases everything when hidden and reloads itself when
  shown or when the PC app restarts.

## Code map

| Module | Role |
|---|---|
| `main.rs` | Command line: GUI, headless, discovery, TV app export |
| `engine.rs` | Runs the pipeline only while someone watches; restarts it when settings change; cast/stop |
| `config.rs` | Settings, presets, persistence, output size and pacing rules |
| `stream.rs` | Packet wire format, fan-out `Hub`, newest-only `Latest` slot |
| `server.rs` | HTTP + HTTPS on one port, WebSocket sessions, player telemetry, per-viewer stats |
| `access.rs` | "Who can watch" policy and access codes |
| `webrtc.rs` | ICE-lite WebRTC peer (str0m), paced UDP sender |
| `tls.rs` | Self-signed server cert (full-duplex rustls), unverified client for the TV |
| `http.rs`, `ws.rs` | Minimal HTTP/1.1 and RFC 6455 (server and client) |
| `codec.rs`, `fmp4.rs` | H.264/HEVC NAL and AV1 OBU parsing, codec strings, fMP4 muxer for MSE |
| `audio.rs`, `clock.rs` | Sample conversion to s16 stereo; the shared capture clock |
| `discovery.rs`, `webos.rs` | SSDP discovery, LG SSAP pairing and app launching |
| `tvapp.rs`, `tvapp/` | The webOS TV app: player plus launch-parameter bootstrap, icons, `appinfo.json` |
| `platform/` | OS interface (`monitors`, `encoders`, `run_video`, `run_audio`); `windows/`: capture, GPU colour conversion, MF encoder, WASAPI |
| `web/player.html`, `web/clock.html` | The TV-side player and the latency-check clock (embedded in the binary) |
| `ui.rs` | egui desktop window |
| `smoke.rs` | Hardware smoke tests (`#[ignore]`d) |

## Tests

```
cargo test                                         # unit tests (no hardware needed)
cargo test --release -- --ignored --skip tv_ --test-threads=1   # hardware smoke tests: capture, NVENC/AMF/QSV, H.264+HEVC+AV1, MSE, restarts
$env:LANCAST_TV="192.168.1.50"; cargo test --release tv_ -- --ignored --nocapture   # cast to a real TV, print latency
```

## Roadmap

- [x] Windows: GPU capture, hardware encode, WebRTC / WebCodecs / MSE players, audio, webOS auto-launch, GUI
- [ ] Adaptive bitrate from WebRTC congestion feedback (for Wi-Fi)
- [ ] DLNA fallback for TVs without a usable browser (MPEG-TS over HTTP, AAC audio)
- [ ] Tray icon + global start/stop hotkey; Wake-on-LAN for the TV
- [ ] Linux backend (PipeWire capture, VA-API encode) and macOS (ScreenCaptureKit, VideoToolbox)
- [ ] HDR capture and HEVC Main10

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
