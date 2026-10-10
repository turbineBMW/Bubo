//! GIF search over KLIPY's API (https://docs.klipy.com).
//!
//! Each user brings their own free API key from partner.klipy.com, set in Preferences — a key
//! can't be shipped in the source, and test keys are capped at 100 requests an hour, which one
//! person searching fits comfortably but a shared key would not.
use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

const API: &str = "https://api.klipy.com/api/v1";
/// Refuse GIFs larger than this — MMS carriers reject big attachments anyway.
pub const MAX_BYTES: usize = 8 * 1024 * 1024;
/// KLIPY allows 8–50; the picker grid shows up to 45.
const PER_PAGE: u32 = 45;
/// How many recently sent GIFs the picker remembers.
const HISTORY_LEN: usize = 45;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Gif {
    /// Direct URL to the animated GIF that gets sent.
    pub url: String,
    /// Small animated GIF for the picker grid.
    pub preview: String,
    /// A still of the preview, shown while it loads; empty if KLIPY had none.
    #[serde(default)]
    pub still: String,
}

#[derive(serde::Deserialize)]
struct Response {
    #[serde(default)]
    result: bool,
    data: Option<Data>,
    errors: Option<serde_json::Value>,
}

#[derive(serde::Deserialize)]
struct Data { data: Vec<Item> }

#[derive(serde::Deserialize)]
struct Item {
    #[serde(default, rename = "type")]
    kind: String,
    /// Size (`hd`, `md`, `sm`, `xs`) → format (`gif`, `jpg`, …) → file.
    #[serde(default)]
    file: HashMap<String, HashMap<String, File>>,
}

#[derive(serde::Deserialize)]
struct File {
    url: String,
    #[serde(default)]
    size: u64,
}

impl Item {
    fn file(&self, size: &str, format: &str) -> Option<&File> {
        self.file.get(size)?.get(format).filter(|f| f.url.starts_with("http"))
    }

    /// The biggest rendition that is still small enough to send, plus a small animated preview
    /// (`sm` is ~220px, sharp in a 112px tile on HiDPI) and its still for the grid.
    /// Ads (`type: "ad"`) are skipped.
    fn to_gif(&self) -> Option<Gif> {
        if self.kind == "ad" { return None; }
        let url = ["hd", "md", "sm", "xs"].iter().filter_map(|s| self.file(s, "gif")).find(|f| f.size as usize <= MAX_BYTES)?.url.clone();
        let preview = ["sm", "xs"].iter().find_map(|s| self.file(s, "gif")).map_or_else(|| url.clone(), |f| f.url.clone());
        let still = ["sm", "xs"].iter().find_map(|s| self.file(s, "jpg")).map(|f| f.url.clone()).unwrap_or_default();
        Some(Gif { url, preview, still })
    }
}

fn http() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| reqwest::Client::builder().timeout(std::time::Duration::from_secs(20)).build().expect("gif http client"))
}

/// KLIPY's `{"errors": {"message": ["…"]}}`, flattened to one line.
fn error_message(errors: &serde_json::Value) -> Option<String> {
    let msgs: Vec<&str> = match errors {
        serde_json::Value::Object(m) => m.values().flat_map(|v| v.as_array().into_iter().flatten().filter_map(|s| s.as_str()).chain(v.as_str())).collect(),
        serde_json::Value::String(s) => vec![s.as_str()],
        _ => vec![],
    };
    (!msgs.is_empty()).then(|| msgs.join(" "))
}

/// Search GIFs; `page` is zero-based.
pub async fn search(api_key: &str, query: &str, page: u32) -> Result<Vec<Gif>> {
    let query = query.trim();
    if query.is_empty() { return Ok(vec![]); }
    let api_key = api_key.trim();
    if api_key.is_empty() { return Err(anyhow!("Add a free KLIPY API key in Preferences to search GIFs")); }
    let resp = http().get(format!("{API}/{api_key}/gifs/search"))
        .query(&[("q", query), ("page", &(page + 1).to_string()), ("per_page", &PER_PAGE.to_string()), ("format_filter", "gif,jpg")])
        .send().await?;
    let status = resp.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS { return Err(anyhow!("KLIPY's hourly search limit was reached — try again later")); }
    let body: Response = resp.json().await.with_context(|| format!("KLIPY replied {status}"))?;
    if !status.is_success() || !body.result {
        let msg = body.errors.as_ref().and_then(error_message).unwrap_or_else(|| format!("HTTP {status}"));
        return Err(anyhow!("KLIPY: {msg}"));
    }
    Ok(body.data.map(|d| d.data.iter().filter_map(Item::to_gif).collect()).unwrap_or_default())
}

fn history_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("dev", "turbinebmw", "bubo").map(|d| d.data_dir().join("gif-history.json"))
}

/// GIFs this user has sent, most recent first. Kept locally; nothing is reported to KLIPY.
pub fn history() -> Vec<Gif> {
    history_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Move `gif` to the front of the history, dropping any older copy and the oldest beyond the cap.
fn push_recent(list: &mut Vec<Gif>, gif: &Gif) {
    list.retain(|g| g.url != gif.url);
    list.insert(0, gif.clone());
    list.truncate(HISTORY_LEN);
}

/// Record a sent GIF in the history.
pub fn remember(gif: &Gif) {
    let Some(path) = history_path() else { return };
    let mut list = history();
    push_recent(&mut list, gif);
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    if let Err(e) = serde_json::to_vec(&list).map_err(anyhow::Error::from).and_then(|b| std::fs::write(&path, b).map_err(Into::into)) {
        tracing::warn!("saving GIF history: {e:#}");
    }
}

/// Download a GIF's bytes, checking that it actually is one.
pub async fn download(url: &str) -> Result<Vec<u8>> {
    let resp = http().get(url).send().await?.error_for_status()?;
    if let Some(len) = resp.content_length() && len as usize > MAX_BYTES { return Err(anyhow!("GIF is too large to send ({} MB)", len / 1024 / 1024)); }
    let bytes = resp.bytes().await?;
    if bytes.len() > MAX_BYTES { return Err(anyhow!("GIF is too large to send")); }
    if !bytes.starts_with(b"GIF8") { return Err(anyhow!("that link is not a GIF")); }
    Ok(bytes.to_vec())
}

/// Fetch a grid preview or still (any image format).
pub async fn thumbnail(url: &str) -> Result<Vec<u8>> {
    Ok(http().get(url).send().await?.error_for_status()?.bytes().await?.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_sendable_gif_and_skips_ads() {
        let body: Response = serde_json::from_str(r#"{"result":true,"data":{"data":[
            {"id":1,"slug":"a","type":"gif","file":{
                "hd":{"gif":{"url":"https://x/hd.gif","width":498,"height":498,"size":9999999}},
                "md":{"gif":{"url":"https://x/md.gif","width":220,"height":220,"size":500000}},
                "sm":{"gif":{"url":"https://x/sm.gif","size":100000},"jpg":{"url":"https://x/sm.jpg","size":5000}}}},
            {"id":2,"type":"ad","file":{"md":{"gif":{"url":"https://ad/md.gif","size":1}}}}
        ],"current_page":1,"per_page":45,"has_next":true}}"#).unwrap();
        let gifs: Vec<Gif> = body.data.unwrap().data.iter().filter_map(Item::to_gif).collect();
        assert_eq!(gifs.len(), 1);
        assert_eq!(gifs[0].url, "https://x/md.gif");
        assert_eq!(gifs[0].preview, "https://x/sm.gif");
        assert_eq!(gifs[0].still, "https://x/sm.jpg");
    }

    #[test]
    fn history_is_newest_first_without_duplicates() {
        let gif = |n: usize| Gif { url: format!("https://x/{n}.gif"), preview: String::new(), still: String::new() };
        let mut list = Vec::new();
        for n in 0..HISTORY_LEN + 5 { push_recent(&mut list, &gif(n)); }
        assert_eq!(list.len(), HISTORY_LEN);
        assert_eq!(list[0], gif(HISTORY_LEN + 4));
        push_recent(&mut list, &gif(HISTORY_LEN));
        assert_eq!(list[0], gif(HISTORY_LEN));
        assert_eq!(list.iter().filter(|g| **g == gif(HISTORY_LEN)).count(), 1);
    }

    #[test]
    fn flattens_error_messages() {
        let body: Response = serde_json::from_str(r#"{"result":false,"errors":{"message":["The provided API key is invalid."]}}"#).unwrap();
        assert_eq!(error_message(&body.errors.unwrap()).as_deref(), Some("The provided API key is invalid."));
    }
}
