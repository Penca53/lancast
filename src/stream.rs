//! Media packets, their wire format, and the fan-out hub that feeds every viewer.

use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// JSON `{codec,width,height,fps,kbps,pacing_kbps}`; cached and replayed to new viewers.
    VideoConfig = 1,
    /// One encoded access unit (Annex-B), or an fMP4 fragment in MSE sessions.
    Video = 2,
    /// JSON `{rate,channels}`; cached and replayed to new viewers.
    AudioConfig = 3,
    /// Interleaved s16le PCM.
    Audio = 4,
    /// fMP4 initialization segment (MSE sessions only).
    VideoInit = 5,
}

impl Kind {
    fn is_config(self) -> bool {
        matches!(self, Kind::VideoConfig | Kind::AudioConfig)
    }
}

#[derive(Clone, Debug)]
pub struct Packet {
    pub kind: Kind,
    pub key: bool,
    /// Capture time on the [`crate::clock`] timeline.
    pub pts_us: u64,
    pub data: Arc<[u8]>,
}

impl Packet {
    pub fn new(kind: Kind, key: bool, pts_us: u64, data: impl Into<Arc<[u8]>>) -> Self {
        Packet { kind, key, pts_us, data: data.into() }
    }

    /// 10-byte header preceding the payload in each WebSocket message:
    /// `kind:u8, flags:u8 (bit0 = keyframe), pts_us:u64 LE`.
    pub fn header(&self) -> [u8; 10] {
        let mut h = [0u8; 10];
        h[0] = self.kind as u8;
        h[1] = self.key as u8;
        h[2..].copy_from_slice(&self.pts_us.to_le_bytes());
        h
    }
}

/// Packets buffered per viewer before it is considered too slow.
const QUEUE: usize = 32;

struct Client {
    id: u64,
    tx: SyncSender<Packet>,
    /// Drop video until the next keyframe (new viewer, or one that fell behind).
    need_key: bool,
    /// A config packet didn't fit in the queue: resend the cached configs as soon as there's
    /// room, before anything else (players can't decode the new stream without them).
    resend_configs: bool,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    clients: Vec<Client>,
    configs: Vec<Packet>,
}

/// Broadcasts packets to all viewers without ever blocking the producer.
#[derive(Default)]
pub struct Hub {
    inner: Mutex<Inner>,
    key_request: AtomicBool,
}

pub struct Subscription {
    pub rx: Receiver<Packet>,
    id: u64,
    hub: Arc<Hub>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.hub.inner.lock().unwrap().clients.retain(|c| c.id != self.id);
    }
}

impl Hub {
    pub fn new() -> Arc<Hub> {
        Arc::default()
    }

    pub fn subscribe(self: &Arc<Self>) -> Subscription {
        let (tx, rx) = sync_channel(QUEUE);
        let mut inner = self.inner.lock().unwrap();
        for c in &inner.configs {
            let _ = tx.try_send(c.clone());
        }
        inner.next_id += 1;
        let id = inner.next_id;
        inner.clients.push(Client { id, tx, need_key: true, resend_configs: false });
        self.request_key();
        Subscription { rx, id, hub: self.clone() }
    }

    pub fn publish(&self, p: Packet) {
        let mut inner = self.inner.lock().unwrap();
        let config = p.kind.is_config();
        if config {
            inner.configs.retain(|c| c.kind != p.kind);
            inner.configs.push(p.clone());
        }
        let video = p.kind == Kind::Video;
        let mut want_key = false;
        let Inner { clients, configs, .. } = &mut *inner;
        clients.retain_mut(|c| {
            if c.resend_configs {
                // `configs` already includes `p` if it is one.
                match configs.iter().try_for_each(|cfg| c.tx.try_send(cfg.clone())) {
                    Ok(()) => c.resend_configs = false,
                    Err(TrySendError::Full(_)) => return true,
                    Err(TrySendError::Disconnected(_)) => return false,
                }
                if config {
                    return true;
                }
            }
            if video && c.need_key && !p.key {
                return true;
            }
            match c.tx.try_send(p.clone()) {
                Ok(()) => {
                    if video {
                        c.need_key = false;
                    }
                    true
                }
                Err(TrySendError::Full(_)) => {
                    if config {
                        // The video that follows a new config needs it: restart at a keyframe.
                        c.resend_configs = true;
                    }
                    if video || config {
                        c.need_key = true;
                        want_key = true;
                    }
                    true
                }
                Err(TrySendError::Disconnected(_)) => false,
            }
        });
        if want_key {
            self.request_key();
        }
    }

    /// Forgets cached configs (call when a pipeline restarts with new parameters).
    pub fn reset(&self) {
        self.inner.lock().unwrap().configs.clear();
    }

    pub fn request_key(&self) {
        self.key_request.store(true, Ordering::Relaxed);
    }

    /// Returns and clears the pending keyframe request.
    pub fn take_key_request(&self) -> bool {
        self.key_request.swap(false, Ordering::Relaxed)
    }

    pub fn viewers(&self) -> usize {
        self.inner.lock().unwrap().clients.len()
    }
}

/// Single-slot mailbox that keeps only the newest value, so a slow consumer
/// always gets the freshest frame instead of a backlog.
pub struct Latest<T> {
    slot: Mutex<(Option<T>, bool)>,
    cv: Condvar,
}

impl<T> Default for Latest<T> {
    fn default() -> Self {
        Latest { slot: Mutex::new((None, false)), cv: Condvar::new() }
    }
}

impl<T> Latest<T> {
    pub fn put(&self, v: T) {
        self.slot.lock().unwrap().0 = Some(v);
        self.cv.notify_one();
    }

    /// Blocks until a value is available; `None` once closed.
    pub fn take(&self) -> Option<T> {
        let mut g = self.slot.lock().unwrap();
        loop {
            if let Some(v) = g.0.take() {
                return Some(v);
            }
            if g.1 {
                return None;
            }
            g = self.cv.wait(g).unwrap();
        }
    }

    pub fn close(&self) {
        self.slot.lock().unwrap().1 = true;
        self.cv.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.slot.lock().unwrap().1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(key: bool, pts: u64) -> Packet {
        Packet::new(Kind::Video, key, pts, vec![pts as u8])
    }

    #[test]
    fn header_layout() {
        let h = Packet::new(Kind::Audio, true, 0x0102030405060708, vec![]).header();
        assert_eq!(h, [4, 1, 8, 7, 6, 5, 4, 3, 2, 1]);
    }

    #[test]
    fn new_viewer_gets_configs_then_waits_for_keyframe() {
        let hub = Hub::new();
        hub.publish(Packet::new(Kind::VideoConfig, false, 0, b"a".to_vec()));
        hub.publish(Packet::new(Kind::VideoConfig, false, 0, b"b".to_vec()));
        let sub = hub.subscribe();
        assert!(hub.take_key_request(), "subscribing must request a keyframe");
        hub.publish(video(false, 1));
        hub.publish(video(true, 2));
        hub.publish(video(false, 3));
        let got: Vec<_> = sub.rx.try_iter().map(|p| (p.kind, p.pts_us)).collect();
        // Only the latest config is replayed; the delta before the keyframe is dropped.
        assert_eq!(got, [(Kind::VideoConfig, 0), (Kind::Video, 2), (Kind::Video, 3)]);
    }

    #[test]
    fn slow_viewer_resyncs_on_keyframe() {
        let hub = Hub::new();
        let sub = hub.subscribe();
        hub.publish(video(true, 0));
        for i in 1..QUEUE as u64 + 5 {
            hub.publish(video(false, i));
        }
        assert!(hub.take_key_request(), "overflow must request a keyframe");
        sub.rx.try_iter().count();
        hub.publish(video(false, 100));
        hub.publish(video(true, 101));
        let got: Vec<_> = sub.rx.try_iter().map(|p| p.pts_us).collect();
        assert_eq!(got, [101]);
    }

    #[test]
    fn config_published_while_queue_is_full_is_resent_before_video() {
        let hub = Hub::new();
        let sub = hub.subscribe();
        for i in 0..QUEUE as u64 {
            hub.publish(video(i == 0, i));
        }
        hub.take_key_request();
        // A pipeline restart while the viewer is backed up: the new config doesn't fit.
        hub.publish(Packet::new(Kind::VideoConfig, false, 0, b"new".to_vec()));
        assert!(hub.take_key_request(), "a lost config must restart video at a keyframe");
        hub.publish(video(true, 100));
        sub.rx.try_iter().count(); // the viewer catches up
        hub.publish(video(false, 101));
        hub.publish(video(true, 102));
        let got: Vec<_> = sub.rx.try_iter().map(|p| (p.kind, p.pts_us)).collect();
        assert_eq!(got, [(Kind::VideoConfig, 0), (Kind::Video, 102)]);
    }

    #[test]
    fn dropped_subscription_is_removed() {
        let hub = Hub::new();
        let a = hub.subscribe();
        let _b = hub.subscribe();
        assert_eq!(hub.viewers(), 2);
        drop(a);
        assert_eq!(hub.viewers(), 1);
    }

    #[test]
    fn latest_keeps_newest_and_closes() {
        let l = Arc::new(Latest::default());
        l.put(1);
        l.put(2);
        assert_eq!(l.take(), Some(2));
        let l2 = l.clone();
        let t = std::thread::spawn(move || l2.take());
        std::thread::sleep(std::time::Duration::from_millis(20));
        l.close();
        assert_eq!(t.join().unwrap(), None);
    }
}
