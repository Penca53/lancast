//! LG webOS TV app: the same player page, packaged to run from the TV itself.
//! Running from `file://` makes it a secure context, which TV browsers require for WebCodecs.

use std::{io, path::Path};

pub const APP_ID: &str = "com.lancast.player";

const PLAYER: &str = include_str!("web/player.html");
const BOOTSTRAP: &str = include_str!("tvapp/bootstrap.html");

/// The app's index.html: the player with the launch-parameter bootstrap injected first.
pub fn index_html() -> String {
    PLAYER.replacen("<head>", &format!("<head>\n{BOOTSTRAP}"), 1)
}

/// Writes the app's files (package them with `ares-package <dir>`).
pub fn export(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("appinfo.json"), include_str!("tvapp/appinfo.json"))?;
    std::fs::write(dir.join("icon.png"), include_bytes!("tvapp/icon.png"))?;
    std::fs::write(dir.join("largeIcon.png"), include_bytes!("tvapp/largeIcon.png"))?;
    std::fs::write(dir.join("index.html"), index_html())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_runs_before_the_player() {
        let html = index_html();
        let (boot, player) = (html.find("window.LANCAST = {").unwrap(), html.find("const app = window.LANCAST").unwrap());
        assert!(boot < player);
        assert_eq!(html.matches("<head>").count(), 1);
    }

    #[test]
    fn export_writes_a_valid_app() {
        let dir = std::env::temp_dir().join(format!("lancast-tvapp-{}", std::process::id()));
        export(&dir).unwrap();
        let info: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("appinfo.json")).unwrap()).unwrap();
        assert_eq!(info["id"], APP_ID);
        for f in ["index.html", "icon.png", "largeIcon.png", info["main"].as_str().unwrap()] {
            assert!(dir.join(f).exists(), "{f} missing");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
