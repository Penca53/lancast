//! "Who can watch": the access policy checked when a player opens the stream, and checked
//! again for open sessions whenever it changes.

use crate::config::{Settings, Visibility};
use std::{
    collections::BTreeSet,
    net::IpAddr,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Clone, Debug, PartialEq)]
pub struct Policy {
    visibility: Visibility,
    code: String,
    cast_targets: BTreeSet<String>,
}

impl Policy {
    pub fn from_settings(s: &Settings) -> Policy {
        Policy { visibility: s.visibility, code: s.access_code.clone(), cast_targets: s.cast_targets.clone() }
    }

    /// Whether a player at `peer` that presents `code` may watch; `this_pc` means it connected
    /// from this machine, which always may.
    pub fn allows(&self, peer: IpAddr, this_pc: bool, code: Option<&str>) -> bool {
        this_pc
            || match self.visibility {
                Visibility::Network => true,
                Visibility::CastTargets => self.cast_targets.contains(&peer.to_string()),
                Visibility::Code => !self.code.is_empty() && code == Some(self.code.as_str()),
                Visibility::ThisPc => false,
            }
    }
}

/// The current policy, shared by the engine (which updates it) and the server.
pub struct Access {
    policy: Mutex<Policy>,
    version: AtomicU64,
}

impl Access {
    pub fn new(policy: Policy) -> Access {
        Access { policy: Mutex::new(policy), version: AtomicU64::new(0) }
    }

    pub fn set(&self, policy: Policy) {
        *self.policy.lock().unwrap() = policy;
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    /// Changes whenever the policy does, so sessions know when to check again.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    pub fn allows(&self, peer: IpAddr, this_pc: bool, code: Option<&str>) -> bool {
        self.policy.lock().unwrap().allows(peer, this_pc, code)
    }
}

impl Default for Access {
    fn default() -> Access {
        Access::new(Policy::from_settings(&Settings::default()))
    }
}

/// A new random access code: 10 characters from an unambiguous alphabet (50 bits).
pub fn new_code() -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut bytes = [0u8; 10];
    getrandom::fill(&mut bytes).expect("OS random number generator");
    bytes.iter().map(|b| ALPHABET[usize::from(b & 31)] as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(visibility: Visibility) -> Policy {
        let mut s = Settings { visibility, access_code: "secret".into(), ..Settings::default() };
        s.cast_targets.insert("10.0.0.2".into());
        Policy::from_settings(&s)
    }

    #[test]
    fn who_may_watch() {
        let (tv, other) = ("10.0.0.2".parse().unwrap(), "10.0.0.3".parse().unwrap());
        let allows = |v, peer, code| policy(v).allows(peer, false, code);
        assert!(allows(Visibility::Network, other, None));
        assert!(allows(Visibility::CastTargets, tv, None));
        assert!(!allows(Visibility::CastTargets, other, None));
        assert!(allows(Visibility::Code, other, Some("secret")));
        assert!(!allows(Visibility::Code, other, Some("guess")));
        assert!(!allows(Visibility::Code, other, None));
        assert!(!allows(Visibility::ThisPc, tv, Some("secret")));
        for v in Visibility::ALL {
            assert!(policy(v).allows(other, true, None), "this PC may always watch ({v:?})");
        }
        let no_code = Policy { code: String::new(), ..policy(Visibility::Code) };
        assert!(!no_code.allows(other, false, Some("")), "an unset code admits nobody");
    }

    #[test]
    fn version_changes_with_the_policy() {
        let a = Access::default();
        let v = a.version();
        a.set(policy(Visibility::ThisPc));
        assert_ne!(a.version(), v);
        assert!(!a.allows("10.0.0.2".parse().unwrap(), false, None));
    }

    #[test]
    fn codes_are_random_and_url_safe() {
        let (a, b) = (new_code(), new_code());
        assert_ne!(a, b);
        assert!(a.len() == 10 && a.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()), "{a}");
    }
}
