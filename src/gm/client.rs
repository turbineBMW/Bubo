//! The Messages-for-Web client: pairing, token refresh, long-poll, and typed RPCs.
use crate::gm::auth::{AuthData, QR_NETWORK};
use crate::gm::proto::config::Config;
use crate::gm::events::Event;
use crate::gm::http;
use crate::gm::proto::authentication::*;
use crate::gm::proto::client::*;
use crate::gm::proto::events::{RpcPairData, UpdateEvents, rpc_pair_data, update_events};
use crate::gm::proto::rpc::*;
use crate::gm::proto::util::EmptyArr;
use crate::gm::proto::{conversations, settings};
use crate::gm::session::{Incoming, Session, auth_message};
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use futures_util::StreamExt;
use prost::Message;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

fn browser_details() -> BrowserDetails {
    BrowserDetails { user_agent: http::USER_AGENT.into(), browser_type: BrowserType::Other as i32, os: "Bubo".into(), device_type: DeviceType::Tablet as i32 }
}

pub struct Client {
    pub auth: Mutex<AuthData>,
    http: reqwest::Client,
    lp_http: reqwest::Client,
    session: Session,
    events: async_channel::Sender<Event>,
    listen_id: AtomicU64,
    /// Number of replayed (old) events the server told us to expect on this listen.
    skip_count: AtomicI64,
    conversations_fetched: AtomicBool,
    refresh_lock: AsyncMutex<()>,
    me: std::sync::Weak<Client>,
    pub(crate) stream_opened: tokio::sync::Notify,
}

pub struct SendOpts {
    pub request_id: Option<String>,
    pub omit_ttl: bool,
    pub custom_ttl: Option<i64>,
    pub dont_encrypt: bool,
    pub message_type: MessageType,
    pub expect_response: bool,
    /// How long to wait for the phone's reply (default 60 s).
    pub timeout: Duration,
}
impl Default for SendOpts {
    fn default() -> Self { Self { request_id: None, omit_ttl: false, custom_ttl: None, dont_encrypt: false, message_type: MessageType::BugleMessage, expect_response: true, timeout: Duration::from_secs(60) } }
}

#[allow(dead_code)]
impl Client {
    pub fn new(auth: AuthData) -> Result<(Arc<Self>, async_channel::Receiver<Event>)> {
        let (tx, rx) = async_channel::unbounded();
        let (http, lp_http) = (http::build_client(false)?, http::build_client(true)?);
        let c = Arc::new_cyclic(|me| Self {
            auth: Mutex::new(auth), http, lp_http,
            session: Session::default(), events: tx, listen_id: AtomicU64::new(0), skip_count: AtomicI64::new(0),
            conversations_fetched: AtomicBool::new(false), refresh_lock: AsyncMutex::new(()), me: me.clone(), stream_opened: tokio::sync::Notify::new(),
        });
        c.session.reset_session_id();
        Ok((c, rx))
    }

    pub(crate) fn emit(&self, e: Event) { let _ = self.events.try_send(e); }
    pub(crate) fn is_google(&self) -> bool { self.auth.lock().unwrap().is_google() }
    fn network(&self) -> &'static str { self.auth.lock().unwrap().network() }
    /// POST through the relay, attaching Google cookies when we have them and absorbing any refreshed ones.
    pub(crate) async fn post(&self, long_poll: bool, url: &str, body: http::Body) -> Result<reqwest::Response> {
        let cookies = self.auth.lock().unwrap().cookie_headers();
        let cli = if long_poll { &self.lp_http } else { &self.http };
        let resp = http::post(cli, url, body, cookies).await?;
        let set: Vec<String> = resp.headers().get_all("set-cookie").iter().filter_map(|v| v.to_str().ok().map(String::from)).collect();
        if !set.is_empty() { self.auth.lock().unwrap().absorb_set_cookies(set.iter().map(String::as_str)); }
        Ok(resp)
    }
    /// GET `/web/config` — the web client's bootstrap; yields our device/session ID.
    pub(crate) async fn fetch_config(&self) -> Result<Config> {
        let cookies = self.auth.lock().unwrap().cookie_headers();
        let mut h = http::relay_headers(None);
        h.remove("x-user-agent"); h.remove("origin");
        h.insert("sec-fetch-site", "same-origin".parse().unwrap());
        let mut req = self.http.get(http::URL_CONFIG).headers(h);
        if let Some((c, a)) = cookies { req = req.header("cookie", c).header("authorization", a); }
        let cfg: Config = http::parse(req.send().await?).await?;
        if let Some(id) = cfg.device_info.as_ref().map(|d| d.device_id.clone()).filter(|s| !s.is_empty()) { self.auth.lock().unwrap().session_id = Some(id); }
        Ok(cfg)
    }
    fn token(&self) -> Vec<u8> { self.auth.lock().unwrap().tachyon_token.clone() }
    pub fn is_paired(&self) -> bool { self.auth.lock().unwrap().is_paired() }
    pub(crate) fn save_auth(&self) { if let Err(e) = self.auth.lock().unwrap().save() { tracing::error!("saving auth: {e:#}"); } }

    // ───────────────────────────── pairing ─────────────────────────────

    /// Begin QR pairing: registers with the relay, starts listening for the phone,
    /// returns the URL to encode into the QR code.
    pub async fn start_pairing(self: &Arc<Self>) -> Result<String> {
        let key = self.auth.lock().unwrap().refresh_key.public_der()?;
        let payload = AuthenticationContainer {
            auth_message: Some(auth_message(uuid::Uuid::new_v4().to_string(), &[], QR_NETWORK)),
            browser_details: Some(browser_details()),
            data: Some(authentication_container::Data::KeyData(KeyData { ecdsa_keys: Some(EcdsaKeys { field1: 2, encrypted_keys: key }), ..Default::default() })),
        };
        let resp: RegisterPhoneRelayResponse = http::parse(self.post(false, &http::url_register_phone_relay(), http::body_proto(&payload)).await?).await?;
        let tok = resp.auth_key_data.as_ref().ok_or_else(|| anyhow!("no auth key data in RegisterPhoneRelay response"))?;
        self.auth.lock().unwrap().update_token(tok);
        let me = self.clone();
        tokio::spawn(async move { me.long_poll_loop(false).await; });
        Ok(self.qr_url(&resp.pairing_key))
    }

    fn qr_url(&self, pairing_key: &[u8]) -> String {
        let a = self.auth.lock().unwrap();
        let d = UrlData { pairing_key: pairing_key.to_vec(), aes_key: a.request_crypto.aes_key.clone(), hmac_key: a.request_crypto.hmac_key.clone() };
        format!("{}{}", http::QR_URL_BASE, B64.encode(d.encode_to_vec()))
    }

    /// Ask for a fresh pairing key (the QR expires); returns a new QR URL.
    pub async fn refresh_pairing(&self) -> Result<String> {
        let payload = AuthenticationContainer { auth_message: Some(auth_message(uuid::Uuid::new_v4().to_string(), &self.token(), QR_NETWORK)), ..Default::default() };
        let resp: RefreshPhoneRelayResponse = http::parse(self.post(false, &http::url_refresh_phone_relay(), http::body_proto(&payload)).await?).await?;
        Ok(self.qr_url(&resp.pair_key))
    }

    fn complete_pairing(self: &Arc<Self>, data: &PairedData) {
        {
            let mut a = self.auth.lock().unwrap();
            if let Some(t) = &data.token_data { a.update_token(t); }
            a.mobile = data.mobile.as_ref().map(Into::into);
            a.browser = data.browser.as_ref().map(Into::into);
        }
        self.save_auth();
        self.emit(Event::Paired { phone_id: data.mobile.as_ref().map(|m| m.source_id.clone()).unwrap_or_default() });
        let me = self.clone();
        tokio::spawn(async move {
            // Give the phone a moment to persist the pairing before we reconnect as a paired browser.
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Err(e) = me.connect().await { tracing::error!("reconnect after pairing: {e:#}"); }
        });
    }

    pub async fn unpair(&self) -> Result<()> {
        if self.is_google() { return self.unpair_gaia().await; }
        let (tok, browser) = { let a = self.auth.lock().unwrap(); (a.tachyon_token.clone(), a.browser_device()) };
        if tok.is_empty() || browser.is_none() { return Ok(()); }
        let payload = RevokeRelayPairingRequest { auth_message: Some(auth_message(uuid::Uuid::new_v4().to_string(), &tok, "")), browser };
        let _: RevokeRelayPairingResponse = http::parse(self.post(false, &http::url_revoke_relay_pairing(), http::body_proto(&payload)).await?).await?;
        Ok(())
    }

    // ───────────────────────────── connection ─────────────────────────────

    /// Connect as a paired browser: refresh the token, open the stream, start ack/ping tasks.
    pub async fn connect(self: &Arc<Self>) -> Result<()> {
        if !self.is_paired() { bail!("not paired"); }
        self.refresh_token().await?;
        self.listen_id.fetch_add(1, Ordering::SeqCst); // kill any previous listener
        let me = self.clone();
        tokio::spawn(async move { me.long_poll_loop(true).await; });
        let me = self.clone();
        tokio::spawn(async move { me.ack_loop().await; });
        let me = self.clone();
        tokio::spawn(async move { me.ping_loop().await; });
        Ok(())
    }

    pub fn disconnect(&self) {
        self.listen_id.fetch_add(1, Ordering::SeqCst);
        self.session.cancel_all();
    }

    async fn refresh_token(&self) -> Result<()> {
        let _g = self.refresh_lock.lock().await;
        let (needs, browser, key) = { let a = self.auth.lock().unwrap(); (a.token_needs_refresh(), a.browser_device(), a.refresh_key.clone()) };
        let Some(browser) = browser else { return Ok(()) };
        if !needs { return Ok(()); }
        let request_id = uuid::Uuid::new_v4().to_string();
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as i64 * 1000;
        let sig = key.sign_refresh(&request_id, ts)?;
        let payload = RegisterRefreshRequest {
            message_auth: Some(auth_message(request_id, &self.token(), self.network())),
            curr_browser_device: Some(browser), unix_timestamp: ts, signature: sig,
            parameters: Some(register_refresh_request::Parameters { empty_arr: Some(EmptyArr {}), more_parameters: None }),
            message_type: 2,
        };
        tracing::debug!("refreshing tachyon token");
        let resp: RegisterRefreshResponse = http::parse(self.post(false, &http::url_register_refresh(), http::body_pblite(&payload)?).await?).await?;
        let tok = resp.token_data.filter(|t| !t.tachyon_auth_token.is_empty()).ok_or_else(|| anyhow!("no token in RegisterRefresh response"))?;
        self.auth.lock().unwrap().update_token(&tok);
        self.save_auth();
        Ok(())
    }

    pub(crate) async fn long_poll_loop_pub(self: Arc<Self>, logged_in: bool) { self.long_poll_loop(logged_in).await }

    async fn long_poll_loop(self: Arc<Self>, logged_in: bool) {
        let my_id = self.listen_id.fetch_add(1, Ordering::SeqCst) + 1;
        let listen_req_id = uuid::Uuid::new_v4().to_string();
        let mut errors = 0u64;
        let mut first = true;
        let mut disconnected_at: Option<std::time::Instant> = None;
        while self.listen_id.load(Ordering::SeqCst) == my_id {
            if logged_in {
                if let Err(e) = self.refresh_token().await {
                    let fatal = e.to_string().contains("HTTP 401") || e.to_string().contains("HTTP 403") || e.to_string().contains("HTTP 404");
                    if fatal { self.emit(Event::ListenFatal(format!("token refresh: {e:#}"))); return; }
                    errors += 1; self.emit(Event::ListenError(format!("token refresh: {e:#}")));
                    tokio::time::sleep(Duration::from_secs((errors + 1) * 5)).await; continue;
                }
            }
            let payload = ReceiveMessagesRequest {
                auth: Some(auth_message(listen_req_id.clone(), &self.token(), self.network())),
                unknown: Some(receive_messages_request::UnknownEmptyObject2 { unknown: Some(receive_messages_request::UnknownEmptyObject1 {}) }),
            };
            let body = match http::body_pblite(&payload) { Ok(b) => b, Err(e) => { tracing::error!("encode: {e:#}"); return; } };
            let resp = match self.post(true, &http::url_receive_messages(self.is_google()), body).await {
                Ok(r) => r,
                Err(e) => { errors += 1; self.emit(Event::ListenError(e.to_string())); tokio::time::sleep(Duration::from_secs((errors + 1) * 5)).await; continue; }
            };
            let status = resp.status().as_u16();
            if status == 401 || status == 403 {
                let b = resp.text().await.unwrap_or_default();
                self.emit(Event::ListenFatal(format!("HTTP {status}: {b}"))); return;
            } else if status >= 400 {
                errors += 1; self.emit(Event::ListenError(format!("HTTP {status}")));
                tokio::time::sleep(Duration::from_secs((errors + 1) * 5)).await; continue;
            }
            if self.listen_id.load(Ordering::SeqCst) != my_id { return; }
            errors = 0;
            tracing::debug!(listen = my_id, "long poll opened");
            self.stream_opened.notify_waiters();
            if logged_in {
                if first { first = false; let me = self.clone(); tokio::spawn(async move { me.post_connect().await; }); }
                else if disconnected_at.map(|t| t.elapsed() > Duration::from_secs(60)).unwrap_or(false) {
                    let me = self.clone(); tokio::spawn(async move { me.get_updates_after_gap().await; });
                }
                self.emit(Event::Connected);
            }
            self.read_stream(resp, my_id).await;
            disconnected_at = Some(std::time::Instant::now());
        }
    }

    /// Consume one ReceiveMessages stream: `[[` then comma-separated pblite arrays, then `]]`.
    async fn read_stream(&self, resp: reqwest::Response, my_id: u64) {
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut opened = false;
        while let Some(chunk) = stream.next().await {
            if self.listen_id.load(Ordering::SeqCst) != my_id { return; }
            let chunk = match chunk { Ok(c) => c, Err(e) => { tracing::warn!("stream read: {e}"); return; } };
            buf.extend_from_slice(&chunk);
            loop {
                // trim leading whitespace / separators
                let mut i = 0;
                while i < buf.len() && (buf[i] == b',' || buf[i].is_ascii_whitespace()) { i += 1; }
                if !opened {
                    if buf.len() < i + 2 { buf.drain(..i); break; }
                    if &buf[i..i + 2] != b"[[" { tracing::warn!("stream did not open with [["); return; }
                    opened = true; i += 1; // keep the inner '[' as the start of the first element
                }
                if i >= buf.len() { buf.clear(); break; }
                if buf[i] == b']' { tracing::debug!("stream end marker"); return; }
                match balanced_json(&buf[i..]) {
                    Some(len) => {
                        let frame = buf[i..i + len].to_vec();
                        buf.drain(..i + len);
                        self.handle_frame(&frame);
                    }
                    None => { buf.drain(..i); break; }
                }
            }
        }
        tracing::debug!("stream closed");
    }

    fn handle_frame(&self, frame: &[u8]) {
        let payload: LongPollingPayload = match crate::gm::pblite::decode(frame) {
            Ok(p) => p, Err(e) => { tracing::warn!("bad frame: {e:#} {}", String::from_utf8_lossy(&frame[..frame.len().min(200)])); return; }
        };
        if let Some(data) = payload.data { self.handle_rpc(data); }
        else if let Some(ack) = payload.ack { let n = ack.count.unwrap_or(0) as i64; tracing::debug!(n, "startup ack count"); self.skip_count.store(n, Ordering::SeqCst); }
        else if payload.heartbeat.is_some() { tracing::trace!("heartbeat"); }
        else if payload.start_read.is_some() { tracing::trace!("startRead"); }
    }

    fn handle_rpc(&self, raw: IncomingRpcMessage) {
        let msg = match self.decode_incoming(raw) {
            Ok(m) => m,
            Err((id, e)) => { tracing::warn!("decode incoming {id}: {e:#}"); self.session.queue_ack(&id); return; }
        };
        self.session.queue_ack(&msg.response_id);
        tracing::debug!(id = %msg.response_id, route = ?msg.route, action = ?msg.action(), session = msg.message.as_ref().map(|m| m.session_id.as_str()).unwrap_or(""),
            enc = msg.message.as_ref().map(|m| m.encrypted_data.len()).unwrap_or(0), unenc = msg.message.as_ref().map(|m| m.unencrypted_data.len()).unwrap_or(0), "← frame");
        if self.session.deliver(&msg, self.is_google()) { return; }
        match msg.route {
            BugleRoute::PairEvent => {
                if let Some(p) = &msg.pair {
                    match &p.event {
                        Some(rpc_pair_data::Event::Paired(d)) => { let me = self.as_arc(); me.complete_pairing(d); }
                        Some(rpc_pair_data::Event::Revoked(_)) => { self.emit(Event::Unpaired); }
                        None => {}
                    }
                }
            }
            BugleRoute::DataEvent => self.handle_update(msg),
            _ => tracing::debug!(route = ?msg.route, "ignoring route"),
        }
    }

    fn decode_incoming(&self, raw: IncomingRpcMessage) -> std::result::Result<Incoming, (String, anyhow::Error)> {
        let id = raw.response_id.clone();
        let route = BugleRoute::try_from(raw.bugle_route).unwrap_or(BugleRoute::Unknown);
        let mut inc = Incoming { response_id: id.clone(), route, is_old: false, pair: None, message: None, decrypted: None };
        let r: Result<()> = (|| {
            match route {
                BugleRoute::PairEvent => inc.pair = Some(RpcPairData::decode(raw.message_data.as_slice())?),
                BugleRoute::DataEvent => {
                    let m = RpcMessageData::decode(raw.message_data.as_slice())?;
                    if !m.encrypted_data.is_empty() {
                        inc.decrypted = Some(self.auth.lock().unwrap().request_crypto.decrypt(&m.encrypted_data)?);
                    }
                    inc.message = Some(m);
                }
                BugleRoute::GaiaEvent => {}
                BugleRoute::Unknown => bail!("unknown bugle route {}", raw.bugle_route),
            }
            Ok(())
        })();
        r.map_err(|e| (id, e))?;
        Ok(inc)
    }

    fn handle_update(&self, mut msg: Incoming) {
        if self.skip_count.load(Ordering::SeqCst) > 0 { self.skip_count.fetch_sub(1, Ordering::SeqCst); msg.is_old = true; }
        if msg.action() != ActionType::GetUpdates { tracing::debug!(action = ?msg.action(), "unsolicited response"); return; }
        if msg.decrypted.is_none() && msg.message.as_ref().map(|m| m.unencrypted_data == [0x72, 0x00]).unwrap_or(false) {
            self.emit(Event::ListenFatal("Google signed this session out".into())); return;
        }
        let Some(dec) = &msg.decrypted else { return };
        let ev = match UpdateEvents::decode(dec.as_slice()) { Ok(e) => e, Err(e) => { tracing::warn!("UpdateEvents decode: {e}"); return; } };
        match ev.event {
            Some(update_events::Event::ConversationEvent(c)) => { if !msg.is_old { for conv in c.data { self.emit(Event::Conversation(conv)); } } }
            Some(update_events::Event::MessageEvent(m)) => { for part in m.data { self.emit(Event::Message { msg: part, is_old: msg.is_old }); } }
            Some(update_events::Event::TypingEvent(t)) => { if !msg.is_old { if let Some(d) = t.data { self.emit(Event::Typing(d)); } } }
            Some(update_events::Event::UserAlertEvent(a)) => { if !msg.is_old { self.emit(Event::Alert(crate::gm::proto::events::AlertType::try_from(a.alert_type).unwrap_or_default())); } }
            Some(update_events::Event::SettingsEvent(s)) => self.emit(Event::Settings(s)),
            Some(update_events::Event::BrowserPresenceCheckEvent(_)) => {
                let me = self.as_arc();
                tokio::spawn(async move { let _ = me.send_rpc(ActionType::AckBrowserPresence, None::<EmptyArr>, SendOpts { expect_response: false, ..Default::default() }).await; });
            }
            Some(update_events::Event::AccountChange(a)) => tracing::info!(?a, "account change"),
            None => tracing::debug!("empty UpdateEvents"),
        }
    }

    fn as_arc(&self) -> Arc<Self> { self.me.upgrade().expect("client dropped") }

    async fn post_connect(self: Arc<Self>) {
        tokio::time::sleep(Duration::from_secs(2)).await;
        self.send_acks().await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        if let Err(e) = self.set_active_session().await { tracing::error!("set active session: {e:#}"); }
    }

    async fn get_updates_after_gap(self: Arc<Self>) {
        let sid = self.session.session_id();
        let _ = self.send_rpc(ActionType::GetUpdates, None::<EmptyArr>, SendOpts { request_id: Some(sid), omit_ttl: true, expect_response: false, ..Default::default() }).await;
    }

    /// GET_UPDATES with a fresh session ID: tells the phone which session receives pushes.
    pub async fn set_active_session(&self) -> Result<()> {
        let sid = self.session.reset_session_id();
        self.send_rpc(ActionType::GetUpdates, None::<EmptyArr>, SendOpts { request_id: Some(sid), omit_ttl: true, expect_response: false, ..Default::default() }).await.map(|_| ())
    }

    async fn ack_loop(self: Arc<Self>) {
        let my_id = self.listen_id.load(Ordering::SeqCst);
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if self.listen_id.load(Ordering::SeqCst) != my_id { return; }
            self.send_acks().await;
        }
    }

    async fn send_acks(&self) {
        let ids = self.session.take_acks();
        if ids.is_empty() { return; }
        let (tok, browser) = { let a = self.auth.lock().unwrap(); (a.tachyon_token.clone(), a.browser_device()) };
        let payload = AckMessageRequest {
            auth_data: Some(auth_message(uuid::Uuid::new_v4().to_string(), &tok, self.network())),
            empty_arr: Some(EmptyArr {}),
            acks: ids.iter().map(|id| ack_message_request::Message { request_id: id.clone(), device: browser.clone() }).collect(),
        };
        let r: Result<OutgoingRpcResponse> = async { http::parse(self.post(false, &http::url_ack_messages(self.is_google()), http::body_pblite(&payload)?).await?).await }.await;
        if let Err(e) = r { tracing::warn!("acks failed, requeueing: {e:#}"); self.session.requeue_acks(ids); }
    }

    /// Ping the phone every minute; report when it stops answering.
    async fn ping_loop(self: Arc<Self>) {
        let my_id = self.listen_id.load(Ordering::SeqCst);
        let mut failures = 0;
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            if self.listen_id.load(Ordering::SeqCst) != my_id { return; }
            let r = tokio::time::timeout(Duration::from_secs(60), self.send_rpc(ActionType::NotifyDittoActivity, Some(NotifyDittoActivityRequest { success: true }), SendOpts::default())).await;
            match r {
                Ok(Ok(_)) => { if failures >= 4 { self.emit(Event::PhoneRespondingAgain); } failures = 0; }
                _ => { failures += 1; if failures == 4 { self.emit(Event::PhoneNotResponding); } if failures == 2 { let _ = self.set_active_session().await; } }
            }
        }
    }

    // ───────────────────────────── RPC ─────────────────────────────

    /// Send an action to the phone. With `expect_response`, waits (≤60 s) for the phone's reply.
    pub async fn send_rpc<M: Message>(&self, action: ActionType, data: Option<M>, opts: SendOpts) -> Result<Option<Incoming>> {
        let request_id = opts.request_id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let (tok, mobile, ttl, crypto, dest) = { let a = self.auth.lock().unwrap(); (a.tachyon_token.clone(), a.mobile_device(), a.tachyon_ttl, a.request_crypto.clone(), a.dest_reg_id.clone()) };
        let raw = data.map(|d| d.encode_to_vec());
        let (unencrypted, encrypted) = match raw { None => (vec![], vec![]), Some(r) if opts.dont_encrypt => (r, vec![]), Some(r) => (vec![], crypto.encrypt(&r)) };
        let inner = OutgoingRpcData { request_id: request_id.clone(), action: action as i32, unencrypted_proto_data: unencrypted, encrypted_proto_data: encrypted, session_id: self.session.session_id() };
        let msg = OutgoingRpcMessage {
            mobile,
            data: Some(outgoing_rpc_message::Data {
                request_id: request_id.clone(), bugle_route: BugleRoute::DataEvent as i32, message_data: inner.encode_to_vec(),
                message_type_data: Some(outgoing_rpc_message::data::Type { empty_arr: Some(EmptyArr {}), message_type: opts.message_type as i32 }),
            }),
            auth: Some(outgoing_rpc_message::Auth { request_id: request_id.clone(), tachyon_auth_token: tok, config_version: Some(crate::gm::auth::config_version()) }),
            ttl: opts.custom_ttl.unwrap_or(if opts.omit_ttl { 0 } else { ttl }),
            dest_registration_i_ds: dest.into_iter().collect(),
        };
        let rx = if opts.expect_response { Some(self.session.wait_for(&request_id)) } else { None };
        tracing::debug!(?action, %request_id, "→ phone");
        let sent: Result<OutgoingRpcResponse> = async { http::parse(self.post(false, &http::url_send_message(self.is_google()), http::body_pblite(&msg)?).await?).await }.await;
        if let Err(e) = sent { self.session.cancel(&request_id); return Err(e); }
        let Some(rx) = rx else { return Ok(None) };
        match tokio::time::timeout(opts.timeout, rx).await {
            Ok(Ok(inc)) => { tracing::debug!(?action, %request_id, "← phone answered"); Ok(Some(inc)) }
            Ok(Err(_)) => { bail!("connection closed before {action:?} was answered") }
            Err(_) => { self.session.cancel(&request_id); bail!("phone did not respond to {action:?} within {:?}", opts.timeout) }
        }
    }

    async fn call<Req: Message, Resp: Message + Default>(&self, action: ActionType, req: Req, user: bool) -> Result<Resp> {
        let _ = user;
        let inc = self.send_rpc(action, Some(req), SendOpts::default()).await?.ok_or_else(|| anyhow!("no response"))?;
        inc.decode::<Resp>().with_context(|| format!("decoding {action:?} response"))
    }

    // ───────────────────────────── typed API ─────────────────────────────

    pub async fn list_conversations(&self, count: i64, folder: list_conversations_request::Folder) -> Result<ListConversationsResponse> {
        // The first fetch after connecting goes out as an "annotation" — mirrors the web client.
        let mt = if self.conversations_fetched.swap(true, Ordering::SeqCst) { MessageType::BugleMessage } else { MessageType::BugleAnnotation };
        let req = ListConversationsRequest { count, folder: folder as i32, cursor: None };
        let inc = self.send_rpc(ActionType::ListConversations, Some(req), SendOpts { message_type: mt, ..Default::default() }).await?.ok_or_else(|| anyhow!("no response"))?;
        inc.decode()
    }
    pub async fn get_conversation(&self, id: &str) -> Result<Option<conversations::Conversation>> {
        Ok(self.call::<_, GetConversationResponse>(ActionType::GetConversation, GetConversationRequest { conversation_id: id.into() }, false).await?.conversation)
    }
    pub async fn list_messages(&self, conversation_id: &str, count: i64, cursor: Option<Cursor>) -> Result<ListMessagesResponse> {
        self.call(ActionType::ListMessages, ListMessagesRequest { conversation_id: conversation_id.into(), count, cursor }, false).await
    }
    pub async fn send_text(&self, conversation_id: &str, participant_id: &str, text: &str, sim: Option<settings::SimPayload>) -> Result<SendMessageResponse> {
        let tmp = uuid::Uuid::new_v4().to_string();
        let req = SendMessageRequest {
            conversation_id: conversation_id.into(),
            message_payload: Some(MessagePayload {
                tmp_id: tmp.clone(), conversation_id: conversation_id.into(), participant_id: participant_id.into(), tmp_id2: tmp.clone(),
                message_payload_content: Some(MessagePayloadContent { message_content: Some(conversations::MessageContent { content: text.into() }) }),
                message_info: vec![conversations::MessageInfo { action_message_id: None, data: Some(conversations::message_info::Data::MessageContent(conversations::MessageContent { content: text.into() })) }],
            }),
            sim_payload: sim, tmp_id: tmp, force_rcs: false, reply: None,
        };
        self.call(ActionType::SendMessage, req, true).await
    }
    pub async fn mark_read(&self, conversation_id: &str, message_id: &str) -> Result<()> {
        self.send_rpc(ActionType::MessageRead, Some(MessageReadRequest { conversation_id: conversation_id.into(), message_id: message_id.into() }), SendOpts::default()).await.map(|_| ())
    }
    pub async fn set_typing(&self, conversation_id: &str, typing: bool) -> Result<()> {
        let req = TypingUpdateRequest { data: Some(typing_update_request::Data { conversation_id: conversation_id.into(), typing }), sim_payload: None };
        self.send_rpc(ActionType::TypingUpdates, Some(req), SendOpts { expect_response: false, ..Default::default() }).await.map(|_| ())
    }
    pub async fn participant_thumbnails(&self, ids: &[String]) -> Result<GetThumbnailResponse> {
        self.call(ActionType::GetParticipantsThumbnail, GetThumbnailRequest { identifiers: ids.to_vec() }, false).await
    }
    pub async fn list_contacts(&self) -> Result<ListContactsResponse> {
        self.call(ActionType::ListContacts, ListContactsRequest { i1: 1, i2: 350, i3: 50 }, false).await
    }
    pub async fn get_or_create_conversation(&self, numbers: &[String]) -> Result<GetOrCreateConversationResponse> {
        let req = GetOrCreateConversationRequest { numbers: numbers.iter().map(|n| conversations::ContactNumber { mysterious_int: 2, number: n.clone(), number2: n.clone(), ..Default::default() }).collect(), ..Default::default() };
        self.call(ActionType::GetOrCreateConversation, req, true).await
    }
    pub async fn is_bugle_default(&self) -> Result<bool> {
        Ok(self.call::<_, IsBugleDefaultResponse>(ActionType::IsBugleDefault, EmptyArr {}, false).await?.success)
    }
}

/// Length of the balanced JSON value at the start of `b`, or None if incomplete.
fn balanced_json(b: &[u8]) -> Option<usize> {
    let mut depth = 0i32; let mut in_str = false; let mut esc = false;
    for (i, &c) in b.iter().enumerate() {
        if in_str { if esc { esc = false } else if c == b'\\' { esc = true } else if c == b'"' { in_str = false } continue; }
        match c { b'"' => in_str = true, b'[' | b'{' => depth += 1, b']' | b'}' => { depth -= 1; if depth == 0 { return Some(i + 1); } } _ => {} }
    }
    None
}
