//! Relay endpoints and the browser-shaped headers Google expects.
use anyhow::{Context, Result, anyhow};
use prost::Message;
use prost_reflect::ReflectMessage;
use reqwest::header::HeaderMap;

pub const API_KEY: &str = "AIzaSyCA4RsOZUFrm9whhtGosPlJLmVPnfSHKz8";
pub const USER_AGENT: &str = "Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36";
const SEC_UA: &str = "\"Google Chrome\";v=\"146\", \"Chromium\";v=\"146\", \"Not-A.Brand\";v=\"24\"";
pub const QR_URL_BASE: &str = "https://support.google.com/messages/?p=web_computer#?c=";

const IM: &str = "https://instantmessaging-pa.googleapis.com";
const IM_G: &str = "https://instantmessaging-pa.clients6.google.com";
const PAIRING: &str = "/$rpc/google.internal.communications.instantmessaging.v1.Pairing";
const MESSAGING: &str = "/$rpc/google.internal.communications.instantmessaging.v1.Messaging";
const REGISTRATION: &str = "/$rpc/google.internal.communications.instantmessaging.v1.Registration";

pub fn url_register_phone_relay() -> String { format!("{IM}{PAIRING}/RegisterPhoneRelay") }
pub fn url_refresh_phone_relay() -> String { format!("{IM}{PAIRING}/RefreshPhoneRelay") }
pub fn url_revoke_relay_pairing() -> String { format!("{IM}{PAIRING}/RevokeRelayPairing") }
pub fn url_receive_messages() -> String { format!("{IM}{MESSAGING}/ReceiveMessages") }
pub fn url_send_message() -> String { format!("{IM}{MESSAGING}/SendMessage") }
pub fn url_ack_messages() -> String { format!("{IM}{MESSAGING}/AckMessages") }
pub fn url_register_refresh() -> String { format!("{IM_G}{REGISTRATION}/RegisterRefresh") }
pub fn url_upload_media() -> String { format!("{IM}/upload") }

pub const CT_PROTOBUF: &str = "application/x-protobuf";
pub const CT_PBLITE: &str = "application/json+protobuf";

pub fn relay_headers(content_type: Option<&str>) -> HeaderMap {
    let mut h = HeaderMap::new();
    let set = |h: &mut HeaderMap, k: &'static str, v: &str| { h.insert(k, v.parse().unwrap()); };
    set(&mut h, "sec-ch-ua", SEC_UA);
    set(&mut h, "x-user-agent", "grpc-web-javascript/0.1");
    set(&mut h, "x-goog-api-key", API_KEY);
    if let Some(ct) = content_type { set(&mut h, "content-type", ct); }
    set(&mut h, "sec-ch-ua-mobile", "?1");
    set(&mut h, "user-agent", USER_AGENT);
    set(&mut h, "sec-ch-ua-platform", "\"Android\"");
    set(&mut h, "accept", "*/*");
    set(&mut h, "origin", "https://messages.google.com");
    set(&mut h, "sec-fetch-site", "cross-site");
    set(&mut h, "sec-fetch-mode", "cors");
    set(&mut h, "sec-fetch-dest", "empty");
    set(&mut h, "referer", "https://messages.google.com/");
    set(&mut h, "accept-language", "en-US,en;q=0.9");
    h
}

pub fn build_client(long_poll: bool) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(10));
    b = if long_poll { b.timeout(std::time::Duration::from_secs(30 * 60)) } else { b.timeout(std::time::Duration::from_secs(120)) };
    Ok(b.build()?)
}

pub enum Body { Proto(Vec<u8>), PbLite(Vec<u8>) }

pub fn body_proto<M: Message>(m: &M) -> Body { Body::Proto(m.encode_to_vec()) }
pub fn body_pblite<M: ReflectMessage>(m: &M) -> Result<Body> { Ok(Body::PbLite(crate::gm::pblite::encode(m)?)) }

/// POST with retries on 5xx; returns raw response.
pub async fn post(cli: &reqwest::Client, url: &str, body: Body) -> Result<reqwest::Response> {
    let (ct, bytes) = match body { Body::Proto(b) => (CT_PROTOBUF, b), Body::PbLite(b) => (CT_PBLITE, b) };
    let mut attempt = 0;
    loop {
        attempt += 1;
        let resp = cli.post(url).headers(relay_headers(Some(ct))).body(bytes.clone()).send().await.with_context(|| format!("POST {url}"))?;
        if resp.status().as_u16() < 500 || attempt >= 3 { return Ok(resp); }
        tracing::debug!(url, status = %resp.status(), attempt, "server error, retrying");
        tokio::time::sleep(std::time::Duration::from_secs(attempt)).await;
    }
}

/// Parse a response in whichever encoding Google chose (binary proto or pblite).
pub async fn parse<M: ReflectMessage + Default>(resp: reqwest::Response) -> Result<M> {
    let status = resp.status();
    let ct = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").split(';').next().unwrap_or("").trim().to_string();
    let url = resp.url().to_string();
    let body = resp.bytes().await?;
    if !status.is_success() {
        let msg = if ct == CT_PBLITE || ct == "text/plain" {
            crate::gm::pblite::decode::<crate::gm::proto::authentication::ErrorResponse>(&body).map(|e| e.message).unwrap_or_default()
        } else { String::new() };
        return Err(anyhow!("{url}: HTTP {status} {msg} [{}]", String::from_utf8_lossy(&body[..body.len().min(300)])));
    }
    match ct.as_str() {
        CT_PROTOBUF => Ok(M::decode(body.as_ref())?),
        CT_PBLITE | "text/plain" => crate::gm::pblite::decode::<M>(&body),
        other => Err(anyhow!("{url}: unknown content-type {other:?}")),
    }
}
