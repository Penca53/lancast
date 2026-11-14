//! Owns the server and runs the capture pipelines only while someone is watching.

use crate::{
    access::{self, Access, Policy},
    config::{Settings, Visibility},
    discovery::local_ip,
    platform, server,
    server::Stats,
    stream::Hub,
    tvapp, webos,
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// Keep the pipeline warm briefly after the last viewer leaves (e.g. a page reload).
const LINGER: Duration = Duration::from_secs(3);

pub struct Engine {
    pub hub: Arc<Hub>,
    pub stats: Arc<Stats>,
    pub port: u16,
    settings: Mutex<Settings>,
    /// Who may watch, as the server checks it; follows `settings`.
    access: Arc<Access>,
    generation: AtomicU64,
    /// App each TV showed before we opened the player, to restore on stop.
    previous_app: Mutex<HashMap<String, String>>,
    /// What the last cast opened on the TV (browser or LanCast app).
    cast_app: Mutex<&'static str>,
}

impl Engine {
    pub fn start(mut settings: Settings) -> std::io::Result<Arc<Engine>> {
        ensure_access_code(&mut settings);
        let access = Arc::new(Access::new(Policy::from_settings(&settings)));
        let hub = Hub::new();
        let stats = Arc::new(Stats::default());
        let mut names = vec!["lancast.local".to_owned(), "localhost".to_owned(), "127.0.0.1".to_owned()];
        names.extend(local_ip("1.1.1.1").map(|ip| ip.to_string()));
        let tls = crate::tls::server_config(crate::config::dir().as_deref(), crate::config::PERSIST, &names).map_err(std::io::Error::other)?;
        let port = server::spawn(settings.port, tls, hub.clone(), stats.clone(), access.clone())?;
        let engine = Arc::new(Engine {
            hub,
            stats,
            port,
            settings: Mutex::new(settings),
            access,
            generation: AtomicU64::new(0),
            previous_app: Mutex::default(),
            cast_app: Mutex::new(webos::BROWSER),
        });
        let e = engine.clone();
        thread::Builder::new().name("supervisor".into()).spawn(move || supervise(&e))?;
        Ok(engine)
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    /// Applies a settings change, persists it, and restarts the pipeline if needed.
    pub fn update(&self, f: impl FnOnce(&mut Settings)) {
        let mut s = self.settings.lock().unwrap();
        let before = s.clone();
        f(&mut s);
        ensure_access_code(&mut s);
        if *s == before {
            return;
        }
        s.save();
        let policy = Policy::from_settings(&s);
        if policy != Policy::from_settings(&before) {
            self.access.set(policy);
        }
        let restart = |a: &Settings| (a.monitor, a.height, a.fps, a.codec, a.rate, a.audio, a.cursor);
        if restart(&s) != restart(&before) {
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Applies settings edited in the UI. The engine's own state (pairing keys, cast targets,
    /// access code) changes while the UI holds an older copy, so the current one is kept.
    pub fn apply_user_settings(&self, mut edited: Settings) {
        self.update(|s| {
            edited.copy_engine_state(s);
            *s = edited;
        });
    }

    /// Replaces the access code: links shared before stop working (open players are cut off).
    pub fn new_access_code(&self) {
        self.update(|s| s.access_code = access::new_code());
    }

    /// Forgets the TVs cast to so far ("Only TVs I cast to" then admits none until the next cast).
    pub fn forget_cast_targets(&self) {
        self.update(|s| s.cast_targets.clear());
    }

    /// Connects to a webOS TV, pairing on first use and remembering the key.
    fn webos(&self, ip: &str, on_prompt: impl FnOnce()) -> anyhow::Result<webos::Session> {
        let key = self.settings().webos_keys.get(ip).cloned();
        let (tv, new_key) = webos::Session::connect(ip, key.as_deref(), on_prompt)?;
        if key.as_deref() != Some(new_key.as_str()) {
            self.update(|s| drop(s.webos_keys.insert(ip.to_owned(), new_key)));
        }
        Ok(tv)
    }

    /// Starts the player on a webOS TV: the LanCast TV app if installed (and enabled),
    /// otherwise the TV's browser. `on_prompt` runs when the TV shows its "allow this
    /// device" dialog (first time only).
    pub fn cast_webos(&self, ip: &str, on_prompt: impl FnOnce()) -> anyhow::Result<()> {
        let mut tv = self.webos(ip, on_prompt)?;
        // Before the player starts, so "Only TVs I cast to" admits it.
        self.update(|s| {
            s.cast_targets.insert(ip.to_owned());
        });
        if let Some(app) = tv.foreground_app() {
            self.previous_app.lock().unwrap().insert(ip.to_owned(), app);
        }
        if self.settings().tv_app {
            let params = serde_json::json!({ "server": self.server_for(Some(ip)), "params": self.player_query() });
            match tv.launch_with(tvapp::APP_ID, params) {
                Ok(()) => {
                    *self.cast_app.lock().unwrap() = tvapp::APP_ID;
                    return Ok(());
                }
                Err(e) => log::info!("LanCast TV app not available ({e:#}); using the browser"),
            }
        }
        *self.cast_app.lock().unwrap() = webos::BROWSER;
        // Same URL every time: the TV re-focuses an existing player tab (which reloads itself
        // when shown) instead of piling up tabs that each hold decoder resources.
        tv.open_url(&self.url_for(Some(ip)))
    }

    /// Ends the cast, returning the TV to whatever it showed before.
    pub fn stop_webos(&self, ip: &str) -> anyhow::Result<()> {
        let previous = self.previous_app.lock().unwrap().remove(ip);
        let cast_app = *self.cast_app.lock().unwrap();
        self.webos(ip, || {})?.leave(cast_app, previous.as_deref())
    }

    /// `ip:port` of this server as reachable from `peer` (picks the right local interface).
    fn server_for(&self, peer: Option<&str>) -> String {
        let ip = local_ip(peer.unwrap_or("192.168.0.1")).map_or("127.0.0.1".into(), |ip| ip.to_string());
        format!("{ip}:{}", self.port)
    }

    /// Player settings as a query string (`mode=…[&hud=1][&k=<access code>]`).
    fn player_query(&self) -> String {
        let s = self.settings();
        let mut q = format!("mode={}", s.player.query());
        if s.tv_stats {
            q += "&hud=1";
        }
        if s.visibility == Visibility::Code {
            q += &format!("&k={}", s.access_code);
        }
        q
    }

    /// Player URL as reachable from `peer`.
    pub fn url_for(&self, peer: Option<&str>) -> String {
        let scheme = if self.settings().https { "https" } else { "http" };
        format!("{scheme}://{}/?{}", self.server_for(peer), self.player_query())
    }
}

/// Creates the access code when the link needs one.
fn ensure_access_code(s: &mut Settings) {
    if s.visibility == Visibility::Code && s.access_code.is_empty() {
        s.access_code = access::new_code();
    }
}

struct Workers {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    generation: u64,
}

impl Workers {
    fn spawn(e: &Engine, generation: u64) -> Workers {
        let s = e.settings();
        let stop = Arc::new(AtomicBool::new(false));
        e.hub.reset();
        let mut threads = vec![worker("video", e, &stop, {
            let s = s.clone();
            move |hub, stats, stop| platform::run_video(&s, hub, stats, stop)
        })];
        if s.audio {
            threads.push(worker("audio", e, &stop, |hub, _, stop| platform::run_audio(hub, stop)));
        }
        Workers { stop, threads, generation }
    }

    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        self.threads.into_iter().for_each(|t| drop(t.join()));
    }
}

/// Runs `f` until stopped, restarting it after errors (e.g. display mode changes).
fn worker<F>(name: &str, e: &Engine, stop: &Arc<AtomicBool>, f: F) -> JoinHandle<()>
where
    F: Fn(&Hub, &Stats, &AtomicBool) -> anyhow::Result<()> + Send + 'static,
{
    let (hub, stats, stop, name2) = (e.hub.clone(), e.stats.clone(), stop.clone(), name.to_owned());
    thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Err(err) = f(&hub, &stats, &stop) {
                    log::error!("{name2} pipeline: {err:#}");
                    stats.set_status(format!("{name2} error: {err:#}"));
                    thread::sleep(Duration::from_secs(1));
                }
            }
        })
        .expect("spawn worker")
}

fn supervise(e: &Engine) {
    let mut running: Option<Workers> = None;
    let mut last_viewer = Instant::now() - LINGER;
    loop {
        thread::sleep(Duration::from_millis(50));
        let generation = e.generation.load(Ordering::Relaxed);
        if e.hub.viewers() > 0 {
            last_viewer = Instant::now();
        }
        let wanted = last_viewer.elapsed() < LINGER;
        let stale = running.as_ref().is_some_and(|w| w.generation != generation);
        if (!wanted || stale) && let Some(w) = running.take() {
            log::info!("stopping pipeline");
            w.stop();
        }
        if wanted && running.is_none() {
            log::info!("starting pipeline");
            running = Some(Workers::spawn(e, generation));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_settings_never_erase_a_pairing_key_saved_meanwhile() {
        let e = Engine::start(Settings { port: 0, ..Settings::default() }).unwrap();
        let ui_copy = e.settings();
        // A cast pairs with a TV while the UI still holds its older copy.
        e.update(|s| drop(s.webos_keys.insert("10.0.0.2".into(), "key".into())));
        e.apply_user_settings(Settings { fps: 30, ..ui_copy });
        let s = e.settings();
        assert_eq!(s.fps, 30);
        assert_eq!(s.webos_keys.get("10.0.0.2").map(String::as_str), Some("key"));
    }

    #[test]
    fn code_visibility_puts_a_stable_code_in_the_link() {
        let e = Engine::start(Settings { port: 0, ..Settings::default() }).unwrap();
        assert!(!e.url_for(None).contains("k="), "no code needed by default");
        let ui_copy = Settings { visibility: Visibility::Code, ..e.settings() };
        e.apply_user_settings(ui_copy.clone());
        let code = e.settings().access_code;
        assert_eq!(code.len(), 10);
        assert!(e.url_for(None).ends_with(&format!("&k={code}")));
        // The UI's copy predates the code: applying it again must keep the code.
        e.apply_user_settings(Settings { fps: 30, ..ui_copy });
        assert_eq!(e.settings().access_code, code);
        e.new_access_code();
        assert_ne!(e.settings().access_code, code);
    }
}
