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
pub const GOOGLE_NETWORK: &str = "GDitto";

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
    /// Google-account pairing: browser cookies for messages.google.com (SAPISID etc.).
    #[serde(default)] pub cookies: std::collections::HashMap<String, String>,
    /// Google-account pairing: registration ID of the phone we're paired to.
    #[serde(default)] pub dest_reg_id: Option<String>,
    #[serde(default)] pub pairing_id: Option<String>,
    /// Web session/device ID from `/web/config`.
    #[serde(default)] pub session_id: Option<String>,
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
               tachyon_token: vec![], tachyon_expiry: 0, tachyon_ttl: 0, cookies: Default::default(), dest_reg_id: None, pairing_id: None, session_id: None }
    }
    /// Fresh pairing state that keeps only the Google cookies: what a re-pair after the phone
    /// expired the session starts from.
    pub fn for_repair(&self) -> Self { let mut a = Self::new(); a.cookies = self.cookies.clone(); a }
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
    pub fn is_paired(&self) -> bool { self.browser.is_some() && !self.tachyon_token.is_empty() && (!self.is_google() || self.pairing_id.is_some()) }
    pub fn is_google(&self) -> bool { self.dest_reg_id.is_some() }
    pub fn has_cookies(&self) -> bool { self.cookies.contains_key("SAPISID") }
    pub fn network(&self) -> &'static str { if self.is_google() { GOOGLE_NETWORK } else { "" } }
    /// `Cookie:` header value plus the SAPISIDHASH `Authorization` Google's APIs want.
    pub fn cookie_headers(&self) -> Option<(String, String)> {
        if self.cookies.is_empty() { return None; }
        let cookie = self.cookies.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ");
        let sapisid = self.cookies.get("SAPISID")?;
        let ts = now_secs();
        use sha1::Digest;
        let h = sha1::Sha1::digest(format!("{ts} {sapisid} https://messages.google.com").as_bytes());
        Some((cookie, format!("SAPISIDHASH {ts}_{}", hex(&h))))
    }
    pub fn absorb_set_cookies<'a>(&mut self, headers: impl Iterator<Item = &'a str>) {
        for h in headers {
            if let Some((kv, _)) = h.split_once(';') { if let Some((k, v)) = kv.split_once('=') { if self.cookies.contains_key(k.trim()) || !self.cookies.is_empty() { self.cookies.insert(k.trim().into(), v.trim().into()); } } }
        }
    }
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

fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }
