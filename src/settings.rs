//! User preferences, stored as JSON next to auth.json.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Which sound the notification daemon is asked to play. Bubo never plays audio itself: the
/// choice travels as a hint on the notification so the shell (and its do-not-disturb logic)
/// stays in charge of whether anything is actually heard.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", content = "path", rename_all = "kebab-case")]
pub enum Sound {
    /// `sound-name = message-new-instant`, resolved from the system sound theme.
    #[default]
    SystemDefault,
    /// `sound-file = <path>`.
    File(PathBuf),
    /// `suppress-sound = true`.
    None,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Settings {
    pub notification_sound: Sound,
}

pub fn path() -> PathBuf {
    directories::ProjectDirs::from("dev", "turbinebmw", "bubo").map(|d| d.config_dir().join("settings.json")).expect("no config dir")
}

impl Settings {
    pub fn load() -> Self {
        std::fs::read(path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    pub fn save(&self) {
        let p = path();
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        if let Err(e) = serde_json::to_vec_pretty(self).map_err(anyhow::Error::from).and_then(|b| std::fs::write(&p, b).map_err(Into::into)) {
            tracing::warn!("saving settings: {e:#}");
        }
    }
}
