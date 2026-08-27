//! Persistent pairing state: `~/.config/bubo/auth.json`.
use crate::gm::crypto::{RefreshKey, RequestCrypto, b64};
use crate::gm::proto::authentication::{ConfigVersion, Device, TokenData};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// What the web client claims to be. Bump when Google's client version moves on.
pub fn config_version() -> ConfigVersion { ConfigVersion { year: 2026, month: 3, day: 18, v1: 4, v2: 6 } }
pub const QR_NETWORK: &str = "Bugle";

#[derive(Clone, Serialize, Deserialize)]
pub struct AuthData {
    pub request_crypto: RequestCrypto,
    pub refresh_key: RefreshKey,
    #[serde(default)] pub browser: Option<DeviceJson>,
    #[serde(default)] pub mobile: Option<DeviceJson>,
    #[serde(default, with = "b64")] pub tachyon_token: Vec<u8>,
    /// Unix seconds.
    #[serde(default)] pub tachyon_expiry: u64,
    /// Microseconds.
    #[serde(default)] pub tachyon_ttl: i64,
}

/// `Device` without prost baggage, for JSON.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct DeviceJson { pub user_id: i64, pub source_id: String, pub network: String }
impl From<&Device> for DeviceJson {
    fn from(d: &Device) -> Self { Self { user_id: d.user_id, source_id: d.source_id.clone(), network: d.network.clone() } }
}
impl From<&DeviceJson> for Device {
    fn from(d: &DeviceJson) -> Self { Self { user_id: d.user_id, source_id: d.source_id.clone(), network: d.network.clone() } }
}

pub fn path() -> PathBuf {
    directories::ProjectDirs::from("dev", "turbinebmw", "bubo").map(|d| d.config_dir().join("auth.json")).expect("no config dir")
}

fn now_secs() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() }

impl AuthData {
    pub fn new() -> Self {
        Self { request_crypto: RequestCrypto::generate(), refresh_key: RefreshKey::generate(), browser: None, mobile: None,
               tachyon_token: vec![], tachyon_expiry: 0, tachyon_ttl: 0 }
    }
    pub fn load() -> Result<Option<Self>> {
        match std::fs::read(path()) { Ok(b) => Ok(Some(serde_json::from_slice(&b)?)), Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None), Err(e) => Err(e.into()) }
    }
    pub fn save(&self) -> Result<()> {
        let p = path();
        std::fs::create_dir_all(p.parent().unwrap())?;
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, p)?;
        Ok(())
    }
    pub fn is_paired(&self) -> bool { self.browser.is_some() && !self.tachyon_token.is_empty() }
    pub fn browser_device(&self) -> Option<Device> { self.browser.as_ref().map(Into::into) }
    pub fn mobile_device(&self) -> Option<Device> { self.mobile.as_ref().map(Into::into) }
    pub fn update_token(&mut self, t: &TokenData) {
        self.tachyon_token = t.tachyon_auth_token.clone();
        let ttl = if t.ttl == 0 { Duration::from_secs(86400) } else { Duration::from_micros(t.ttl as u64) };
        self.tachyon_expiry = now_secs() + ttl.as_secs();
        self.tachyon_ttl = ttl.as_micros() as i64;
    }
    /// Refresh when less than an hour of validity remains.
    pub fn token_needs_refresh(&self) -> bool { self.tachyon_expiry.saturating_sub(now_secs()) < 3600 }
}
