//! User settings, quality presets and persistence.

use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

/// Video codec. All are encoded as 8-bit 4:2:0 (Main profiles), which TV decoders support.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    pub const ALL: [Codec; 3] = [Codec::H264, Codec::Hevc, Codec::Av1];

    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
            Codec::Av1 => "AV1",
        }
    }

    /// Command-line name.
    pub fn name(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
            Codec::Av1 => "av1",
        }
    }

    /// The codec an RFC 6381 codec string (`avc1.…`, `hvc1.…`, `av01.…`) names.
    pub fn from_codec_string(s: &str) -> Option<Codec> {
        Codec::ALL.into_iter().find(|c| s.starts_with(c.fourcc()))
    }

    /// MP4 sample entry type, also the prefix of the codec string.
    pub fn fourcc(self) -> &'static str {
        match self {
            Codec::H264 => "avc1",
            Codec::Hevc => "hvc1",
            Codec::Av1 => "av01",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum RateControl {
    /// Constant bitrate: predictable latency, best for constrained networks.
    Cbr { kbps: u32 },
    /// Constant quality (0-100): bitrate follows content, best on wired LAN.
    Quality { level: u32 },
}

impl RateControl {
    /// Expected peak bitrate.
    pub fn kbps(self) -> u32 {
        match self {
            RateControl::Cbr { kbps } => kbps,
            RateControl::Quality { .. } => 100_000,
        }
    }

    /// Rate at which WebRTC packets are released for this stream (see [`pacing_kbps`]).
    pub fn pacing_kbps(self) -> u32 {
        match self {
            RateControl::Cbr { kbps } => pacing_kbps(kbps),
            // No known bitrate (100 Mbps is only a guess at the peak): stay under 100 Mbit TV
            // ports instead of pacing above them for a rate the stream may never reach.
            RateControl::Quality { .. } => PORT_SAFE_KBPS,
        }
    }
}

/// Fastest pacing that doesn't overflow 100 Mbit TV ports (common on LG/Samsung).
const PORT_SAFE_KBPS: u32 = 95_000;

/// Rate at which UDP (WebRTC) packets are released for a stream of `kbps`: well above the
/// stream rate so frames go out quickly, but under 95 Mbps unless the stream itself needs
/// more, so bursts don't overflow 100 Mbit TV ports and switch buffers.
pub fn pacing_kbps(kbps: u32) -> u32 {
    (kbps * 6 / 5).max((kbps * 4).min(PORT_SAFE_KBPS))
}

/// How the TV-side page decodes video.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Player {
    /// Best available: WebCodecs (secure pages), then WebRTC, then Media Source Extensions.
    Auto,
    WebCodecs,
    WebRtc,
    Mse,
}

impl Player {
    pub fn query(self) -> &'static str {
        match self {
            Player::Auto => "auto",
            Player::WebCodecs => "webcodecs",
            Player::WebRtc => "webrtc",
            Player::Mse => "mse",
        }
    }
}

/// Who may open the stream. This PC can always watch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Visibility {
    /// Any device on the network.
    Network,
    /// TVs LanCast has cast to (remembered by IP).
    CastTargets,
    /// Anyone with the link, which carries a random access code.
    Code,
    ThisPc,
}

impl Visibility {
    pub const ALL: [Visibility; 4] = [Visibility::Network, Visibility::CastTargets, Visibility::Code, Visibility::ThisPc];

    pub fn label(self) -> &'static str {
        match self {
            Visibility::Network => "Anyone on the network",
            Visibility::CastTargets => "Only TVs I cast to",
            Visibility::Code => "Anyone with the link",
            Visibility::ThisPc => "This PC only",
        }
    }

    /// Command-line name.
    pub fn name(self) -> &'static str {
        match self {
            Visibility::Network => "network",
            Visibility::CastTargets => "cast",
            Visibility::Code => "code",
            Visibility::ThisPc => "pc",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Preset {
    UltraLan,
    Balanced,
    WiFi,
}

impl Preset {
    pub const ALL: [Preset; 3] = [Preset::UltraLan, Preset::Balanced, Preset::WiFi];

    pub fn label(self) -> &'static str {
        match self {
            Preset::UltraLan => "Ultra (wired LAN)",
            Preset::Balanced => "Balanced",
            Preset::WiFi => "Wi-Fi",
        }
    }

    /// Applies the preset's video parameters, leaving unrelated settings untouched.
    pub fn apply(self, s: &mut Settings) {
        let (height, fps, kbps) = match self {
            // Measured on an LG webOS TV (100 Mbit Ethernet, browser MSE player) with constant motion:
            // 25 Mbps at 1440p60 plays with ~0.12 s of buffer; 40 Mbps needs ~0.25 s; 50+ Mbps stalls.
            Preset::UltraLan => (0, 60, 25_000),
            Preset::Balanced => (1080, 60, 15_000),
            Preset::WiFi => (1080, 30, 10_000),
        };
        s.preset = Some(self);
        s.height = height;
        s.fps = fps;
        s.rate = RateControl::Cbr { kbps };
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub preset: Option<Preset>,
    /// Monitor to capture by its Windows device name (e.g. `DISPLAY2`), which survives monitors
    /// being added, removed or reordered; empty = the primary one.
    pub display: String,
    /// Output height, scaling up or down with the aspect ratio kept; 0 = native resolution.
    #[serde(alias = "max_height")]
    pub height: u32,
    pub fps: u32,
    pub codec: Codec,
    pub rate: RateControl,
    pub audio: bool,
    pub cursor: bool,
    pub player: Player,
    /// Serve the player over HTTPS (self-signed): browsers only enable the low-latency
    /// WebCodecs decoder on secure pages. Off by default: some TV browsers (LG webOS 25)
    /// show a certificate warning whose "Proceed" never loads the page.
    pub https: bool,
    /// Show the stats overlay (with the live PC clock for latency checks) on the TV.
    pub tv_stats: bool,
    /// Use the LanCast TV app (webOS) when it's installed, instead of the TV's browser.
    pub tv_app: bool,
    pub port: u16,
    /// Manually added TV addresses.
    pub manual_devices: Vec<String>,
    /// Who may watch.
    pub visibility: Visibility,
    /// Code required in the link with [`Visibility::Code`]; created when first needed.
    pub access_code: String,
    /// IPs of TVs we've cast to, allowed with [`Visibility::CastTargets`].
    pub cast_targets: BTreeSet<String>,
    /// webOS pairing keys by TV IP, so the TV only asks once.
    pub webos_keys: BTreeMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        let mut s = Settings {
            preset: None,
            display: String::new(),
            height: 0,
            fps: 60,
            codec: Codec::H264,
            rate: RateControl::Cbr { kbps: 25_000 },
            audio: true,
            cursor: true,
            player: Player::Auto,
            https: false,
            tv_stats: false,
            tv_app: true,
            port: 7878,
            manual_devices: Vec::new(),
            visibility: Visibility::Network,
            access_code: String::new(),
            cast_targets: BTreeSet::new(),
            webos_keys: BTreeMap::new(),
        };
        Preset::UltraLan.apply(&mut s);
        s
    }
}

/// Per-user config directory (settings, TLS certificate).
pub fn dir() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("lancast"))
}

/// Whether we may write to [`dir`]: tests must never touch the user's real files.
pub const PERSIST: bool = !cfg!(test);

impl Settings {
    /// Copies the state the engine maintains itself (pairing keys, cast targets, access code)
    /// from `from`: it changes behind the UI's back, so the UI must never overwrite it.
    pub fn copy_engine_state(&mut self, from: &Settings) {
        self.webos_keys.clone_from(&from.webos_keys);
        self.cast_targets.clone_from(&from.cast_targets);
        self.access_code.clone_from(&from.access_code);
    }

    fn path() -> Option<PathBuf> {
        Some(dir()?.join("settings.json"))
    }

    pub fn load() -> Settings {
        let Some(bytes) = Self::path().and_then(|p| std::fs::read(p).ok()) else { return Settings::default() };
        Self::parse(&bytes).unwrap_or_else(|e| {
            log::warn!("ignoring unreadable settings file: {e}");
            Settings::default()
        })
    }

    /// Parses settings JSON, tolerating the UTF-8 byte-order mark Windows editors add.
    fn parse(bytes: &[u8]) -> serde_json::Result<Settings> {
        serde_json::from_slice(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes))
    }

    pub fn save(&self) {
        let Some(path) = Self::path().filter(|_| PERSIST) else { return };
        let write = || -> std::io::Result<()> {
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(&path, serde_json::to_vec_pretty(self)?)
        };
        if let Err(e) = write() {
            log::warn!("saving settings: {e}");
        }
    }
}

/// Output size for a `w`x`h` source scaled (up or down) to `height` (0 = native),
/// preserving aspect ratio and rounding to even dimensions as encoders require.
pub fn fit(w: u32, h: u32, height: u32) -> (u32, u32) {
    let even = |v: u32| (v & !1).max(2);
    if height == 0 || height == h {
        return (even(w), even(h));
    }
    let scaled_w = (w as u64 * height as u64 + h as u64 / 2) / h as u64;
    (even(scaled_w as u32), even(height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_native_and_upscale() {
        assert_eq!(fit(2560, 1440, 0), (2560, 1440));
        assert_eq!(fit(2560, 1440, 1440), (2560, 1440));
        assert_eq!(fit(2560, 1440, 2160), (3840, 2160));
        assert_eq!(fit(1920, 1080, 2160), (3840, 2160));
    }

    #[test]
    fn fit_downscales_preserving_aspect() {
        assert_eq!(fit(2560, 1440, 1080), (1920, 1080));
        assert_eq!(fit(3440, 1440, 1080), (2580, 1080));
        assert_eq!(fit(1366, 768, 720), (1280, 720));
    }

    #[test]
    fn fit_rounds_to_even() {
        assert_eq!(fit(1365, 767, 0), (1364, 766));
        let (w, h) = fit(1000, 999, 333);
        assert_eq!((w % 2, h % 2), (0, 0));
    }

    #[test]
    fn pacing_stays_above_stream_rate_and_under_100mbit_when_possible() {
        assert_eq!(pacing_kbps(10_000), 40_000);
        assert_eq!(pacing_kbps(50_000), 95_000);
        assert_eq!(pacing_kbps(150_000), 180_000);
        for kbps in [1_000, 30_000, 63_000, 79_000] {
            let p = pacing_kbps(kbps);
            assert!(p >= kbps * 6 / 5 && p <= 95_000, "{kbps} -> {p}");
        }
        assert_eq!(RateControl::Cbr { kbps: 25_000 }.pacing_kbps(), pacing_kbps(25_000));
        assert!(RateControl::Quality { level: 85 }.pacing_kbps() <= 95_000, "unknown bitrate: stay under 100 Mbit");
    }

    #[test]
    fn codec_names() {
        for c in Codec::ALL {
            assert_eq!(Codec::from_codec_string(&format!("{}.whatever", c.fourcc())), Some(c));
        }
        assert_eq!(Codec::from_codec_string("vp09.00.10.08"), None);
    }

    #[test]
    fn presets_apply() {
        let mut s = Settings::default();
        Preset::WiFi.apply(&mut s);
        assert_eq!((s.height, s.fps), (1080, 30));
        assert_eq!(s.rate, RateControl::Cbr { kbps: 10_000 });
        assert_eq!(s.preset, Some(Preset::WiFi));
    }

    #[test]
    fn settings_roundtrip_and_tolerate_missing_fields() {
        let mut s = Settings::default();
        s.webos_keys.insert("10.0.0.2".into(), "abc".into());
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&json).unwrap(), s);
        let partial: Settings = serde_json::from_str(r#"{"fps":120,"max_height":720}"#).unwrap();
        assert_eq!((partial.fps, partial.height), (120, 720), "old field name still loads");
        let with_bom = Settings::parse(b"\xEF\xBB\xBF{\"fps\":30}").unwrap();
        assert_eq!(with_bom.fps, 30, "UTF-8 BOM (e.g. from Notepad/PowerShell) is accepted");
        assert_eq!(partial.port, 7878);
        let old_monitor_index: Settings = serde_json::from_str(r#"{"monitor":1,"fps":30}"#).unwrap();
        assert_eq!((old_monitor_index.fps, old_monitor_index.display.as_str()), (30, ""), "old index ignored: primary");
    }
}
