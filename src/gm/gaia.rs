//! Google-account pairing ("Gaia"): sign in with browser cookies, then run a UKEY2
//! handshake with the phone. Both sides derive the same emoji from the shared secret;
//! the user confirms it on the phone, and the session keys fall out of the handshake.
use crate::gm::auth::GOOGLE_NETWORK;
use crate::gm::client::{Client, SendOpts};
use crate::gm::events::Event;
use crate::gm::http;
use crate::gm::proto::authentication::*;
use crate::gm::proto::rpc::{ActionType, MessageType};
use crate::gm::proto::ukey::*;
use crate::gm::session::auth_message;
use anyhow::{Context, Result, anyhow, bail};
use hkdf::Hkdf;
use p256::ecdh::EphemeralSecret;
use p256::{EncodedPoint, PublicKey};
use prost::Message;
use sha2::{Digest, Sha256, Sha512};
use std::sync::Arc;
use std::time::Duration;

fn browser_details() -> BrowserDetails {
    BrowserDetails { user_agent: http::USER_AGENT.into(), browser_type: BrowserType::Other as i32, os: "Bubo".into(), device_type: DeviceType::Tablet as i32 }
}

fn hkdf32(key: &[u8], salt: &[u8], info: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; 32];
    Hkdf::<Sha256>::new(Some(salt), key).expand(info, &mut out).unwrap();
    out
}

const ENCRYPTION_KEY_INFO: [u8; 32] = [130, 170, 85, 160, 211, 151, 248, 131, 70, 202, 28, 238, 141, 57, 9, 185, 95, 19, 250, 125, 235, 29, 74, 179, 131, 118, 184, 37, 109, 168, 85, 16];

const EMOJIS_V0: &[&str] = &["😁", "😅", "🤣", "🫠", "🥰", "😇", "🤩", "😘", "😜", "🤗", "🤔", "🤐", "😴", "🥶", "🤯", "🤠", "🥳", "🥸", "😎", "🤓", "🧐", "🥹", "😭", "😱", "😖", "🥱", "😮‍💨", "🤡", "💩", "👻", "👽", "🤖", "😻", "💌", "💘", "💕", "❤", "💢", "💥", "💫", "💬", "🗯", "💤", "👋", "🙌", "🙏", "✍", "🦶", "👂", "🧠", "🦴", "👀", "🧑", "🧚", "🧍", "👣", "🐵", "🐶", "🐺", "🦊", "🦁", "🐯", "🦓", "🦄", "🐑", "🐮", "🐷", "🐿", "🐰", "🦇", "🐻", "🐨", "🐼", "🦥", "🐾", "🐔", "🐥", "🐦", "🕊", "🦆", "🦉", "🪶", "🦩", "🐸", "🐢", "🦎", "🐍", "🐳", "🐬", "🦭", "🐠", "🐡", "🦈", "🪸", "🐌", "🦋", "🐛", "🐝", "🐞", "🪱", "💐", "🌸", "🌹", "🌻", "🌱", "🌲", "🌴", "🌵", "🌾", "☘", "🍁", "🍂", "🍄", "🪺", "🍇", "🍈", "🍉", "🍋", "🍌", "🍍", "🍎", "🍐", "🍒", "🍓", "🥝", "🥥", "🥑", "🥕", "🌽", "🌶", "🫑", "🥦", "🥜", "🍞", "🥐", "🥨", "🧀", "🍗", "🍔", "🍟", "🍕", "🌭", "🌮", "🥗", "🥣", "🍿", "🦀", "🦑", "🍦", "🍩", "🍪", "🍫", "🍰", "🍬", "🍭", "☕", "🫖", "🍹", "🥤", "🧊", "🥢", "🍽", "🥄", "🧭", "🏔", "🌋", "🏕", "🏖", "🪵", "🏗", "🏡", "🏰", "🛝", "🚂", "🛵", "🛴", "🛼", "🚥", "⚓", "🛟", "⛵", "✈", "🚀", "🛸", "🧳", "⏰", "🌙", "🌡", "🌞", "🪐", "🌠", "🌧", "🌀", "🌈", "☂", "⚡", "❄", "⛄", "🔥", "🎇", "🧨", "✨", "🎈", "🎉", "🎁", "🏆", "🏅", "⚽", "⚾", "🏀", "🏐", "🏈", "🎾", "🎳", "🏓", "🥊", "⛳", "⛸", "🎯", "🪁", "🔮", "🎮", "🧩", "🧸", "🪩", "🖼", "🎨", "🧵", "🧶", "🦺", "🧣", "🧤", "🧦", "🎒", "🩴", "👟", "👑", "👒", "🎩", "🧢", "💎", "🔔", "🎤", "📻", "🎷", "🪗", "🎸", "🎺", "🎻", "🥁", "📺", "🔋", "💻", "💿", "☎", "🕯", "💡", "📖", "📚", "📬", "✏", "✒", "🖌", "🖍", "📝", "💼", "📋", "📌", "📎", "🔑", "🔧", "🧲", "🪜", "🧬", "🔭", "🩹", "🩺", "🪞", "🛋", "🪑", "🛁", "🧹", "🧺", "🔱", "🏁", "🐪", "🐘", "🦃", "🍞", "🍜", "🍠", "🚘", "🤿", "🃏", "👕", "📸", "🏷", "✂", "🧪", "🚪", "🧴", "🧻", "🪣", "🧽", "🚸"];
const EMOJIS_ADDED_V1: &[&str] = &["🍋‍🟩", "🐦‍🔥", "🐲", "🪅", "🦜", "🏺", "🗿", "🫐", "⛽", "🍱", "🥡", "🧋", "🍼", "📐"];
const EMOJIS_REMOVED_V1: &[&str] = &["💻", "🤗", "💬", "👋", "😁", "😎", "😇", "🥰", "🤓", "🤩"];

fn emojis_v1() -> Vec<&'static str> {
    let mut v: Vec<&str> = Vec::new();
    for e in EMOJIS_V0 { if !v.contains(e) { v.push(e); } } // dedupe, order preserved
    v.extend_from_slice(EMOJIS_ADDED_V1);
    v.retain(|e| !EMOJIS_REMOVED_V1.contains(e));
    v
}

/// One handshake attempt.
pub struct PairingSession {
    pub id: String,
    start_ms: i64,
    secret: EphemeralSecret,
    init_payload: Vec<u8>,
    finish_payload: Vec<u8>,
    server_init: Option<GaiaPairingResponseContainer>,
    next_key: Vec<u8>,
    pub dest_unknown_int: u64,
}

impl PairingSession {
    fn new(dest_unknown_int: u64) -> Self {
        let secret = EphemeralSecret::random(&mut crate::gm::crypto::rand_core06());
        let pk = EncodedPoint::from(secret.public_key());
        let (x, y) = (pk.x().unwrap(), pk.y().unwrap());
        let pubkey = GenericPublicKey { r#type: PublicKeyType::EcP256 as i32, public_key: Some(generic_public_key::PublicKey::EcP256PublicKey(EcP256PublicKey {
            x: [&[0u8][..], x.as_slice()].concat(), y: [&[0u8][..], y.as_slice()].concat() })) };
        let finish_payload = Ukey2Message { message_type: ukey2_message::Type::ClientFinish as i32, message_data: Ukey2ClientFinished { public_key: Some(pubkey) }.encode_to_vec() }.encode_to_vec();
        let commitment = Sha512::digest(&finish_payload).to_vec();
        let mut random = vec![0u8; 32]; { use rand::RngCore; rand::rng().fill_bytes(&mut random); }
        let init = Ukey2ClientInit { version: 1, random, next_protocol: "AES_256_CBC-HMAC_SHA256".into(),
            cipher_commitments: vec![ukey2_client_init::CipherCommitment { handshake_cipher: Ukey2HandshakeCipher::P256Sha512 as i32, commitment }] };
        let init_payload = Ukey2Message { message_type: ukey2_message::Type::ClientInit as i32, message_data: init.encode_to_vec() }.encode_to_vec();
        let start_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
        Self { id: uuid::Uuid::new_v4().to_string(), start_ms, secret, init_payload, finish_payload, server_init: None, next_key: vec![], dest_unknown_int }
    }

    /// Verify the phone's SERVER_INIT, derive the auth key, and return the emoji to show.
    fn process_server_init(&mut self, resp: GaiaPairingResponseContainer) -> Result<String> {
        let um = Ukey2Message::decode(resp.data.as_slice()).context("server init envelope")?;
        if um.message_type != ukey2_message::Type::ServerInit as i32 { bail!("unexpected UKEY2 message type {}", um.message_type); }
        let si = Ukey2ServerInit::decode(um.message_data.as_slice()).context("server init")?;
        if si.version != 1 { bail!("server init version {}", si.version); }
        if si.handshake_cipher != Ukey2HandshakeCipher::P256Sha512 as i32 { bail!("handshake cipher {}", si.handshake_cipher); }
        if si.random.len() != 32 { bail!("server random length {}", si.random.len()); }
        let Some(generic_public_key::PublicKey::EcP256PublicKey(k)) = si.public_key.and_then(|p| p.public_key) else { bail!("server key is not P-256") };
        let strip = |v: &[u8]| -> Result<[u8; 32]> {
            let v = if v.len() == 33 { if v[0] != 0 { bail!("bad coordinate prefix") } &v[1..] } else { v };
            Ok(<[u8; 32]>::try_from(v).map_err(|_| anyhow!("coordinate length {}", v.len()))?)
        };
        let (x, y) = (strip(&k.x)?, strip(&k.y)?);
        let server_pk = PublicKey::from_sec1_bytes(&[&[4u8][..], &x, &y].concat()).context("server public key")?;
        let dh = self.secret.diffie_hellman(&server_pk);
        let shared = Sha256::digest(dh.raw_secret_bytes());
        let auth_info = [self.init_payload.as_slice(), resp.data.as_slice()].concat();
        let auth = hkdf32(&shared, b"UKEY2 v1 auth", &auth_info);
        self.next_key = hkdf32(&shared, b"UKEY2 v1 next", &auth_info);
        let n = u32::from_be_bytes([auth[0], auth[1], auth[2], auth[3]]) as usize;
        let emoji = match resp.confirmed_verification_code_version {
            0 => EMOJIS_V0[n % EMOJIS_V0.len()].to_string(),
            1 => { let v = emojis_v1(); v[n % v.len()].to_string() }
            v => bail!("unsupported verification code version {v}"),
        };
        self.server_init = Some(resp);
        Ok(emoji)
    }

    /// After the phone confirms: the AES/HMAC keys for the session.
    fn derive_keys(&self) -> Result<(Vec<u8>, Vec<u8>)> {
        let client = hkdf32(&self.next_key, &ENCRYPTION_KEY_INFO, b"client");
        let server = hkdf32(&self.next_key, &ENCRYPTION_KEY_INFO, b"server");
        match self.server_init.as_ref().map(|s| s.confirmed_key_derivation_version).unwrap_or(0) {
            0 => Ok((client, server)),
            1 => {
                let (a, b) = if java_hash(&client) < java_hash(&server) { (&client, &server) } else { (&server, &client) };
                let h = Sha256::digest([&ENCRYPTION_KEY_INFO[..], a, b].concat());
                Ok((hkdf32(&h, b"Ditto salt 1", b"Ditto info 1"), hkdf32(&h, b"Ditto salt 2", b"Ditto info 2")))
            }
            v => bail!("unsupported key derivation version {v}"),
        }
    }
}

/// `Arrays.hashCode` for signed bytes — the phone sorts keys by this.
fn java_hash(b: &[u8]) -> i32 { b.iter().fold(1i32, |h, &x| h.wrapping_mul(31).wrapping_add(x as i8 as i32)) }

/// Field 8 of GaiaPairingRequestContainer. The phone checks for this exact text on CLIENT_FINISHED.
const PRIVATE_API_CONFIRMATION: &str = "This is an undocumented API. Use or access of undocumented Google APIs without express authorization is prohibited per the Google API Terms of Service (https://developers.google.com/terms).";

impl Client {
    fn base_sign_in(&self) -> Result<SignInGaiaRequest> {
        let sid = self.auth.lock().unwrap().session_id.clone().ok_or_else(|| anyhow!("no session id; fetch config first"))?;
        let hex: String = sid.chars().filter(|c| *c != '-').collect();
        Ok(SignInGaiaRequest {
            auth_message: Some(auth_message(uuid::Uuid::new_v4().to_string(), &[], GOOGLE_NETWORK)),
            inner: Some(sign_in_gaia_request::Inner { device_id: Some(sign_in_gaia_request::inner::DeviceId { unknown_int1: 3, device_id: format!("messages-web-{hex}") }), some_data: None }),
            unknown_int3: 0, network: GOOGLE_NETWORK.into(),
        })
    }

    async fn sign_in_gaia(&self) -> Result<SignInGaiaResponse> {
        let mut req = self.base_sign_in()?;
        let key = self.auth.lock().unwrap().refresh_key.public_der()?;
        req.inner.as_mut().unwrap().some_data = Some(sign_in_gaia_request::inner::Data { some_data: key });
        let resp: SignInGaiaResponse = http::parse(self.post(false, &http::url_sign_in_gaia(), http::body_pblite(&req)?).await?).await?;
        let dev = resp.device_data.as_ref().and_then(|d| d.device_wrapper.as_ref()).and_then(|w| w.device.clone()).ok_or_else(|| anyhow!("SignInGaia: no device in response"))?;
        {
            let mut a = self.auth.lock().unwrap();
            if let Some(t) = &resp.token_data { a.update_token(t); }
            a.browser = Some((&dev).into());
            let mut mobile = dev.clone(); mobile.source_id = mobile.source_id.to_lowercase();
            a.mobile = Some((&mobile).into());
        }
        Ok(resp)
    }

    /// Phase 1: sign in, pick the phone, run CLIENT_INIT. Returns the emoji to show the user.
    pub async fn start_gaia_pairing(self: &Arc<Self>) -> Result<(String, PairingSession)> {
        if !self.auth.lock().unwrap().has_cookies() { bail!("Google-account pairing needs browser cookies (SAPISID)"); }
        self.fetch_config().await.context("fetching web config")?;
        let resp = self.sign_in_gaia().await.context("SignInGaia")?;
        let dd = resp.device_data.unwrap_or_default();
        let mut primaries: Vec<(String, u64, i64)> = dd.unknown_items2.iter().filter(|d| d.unknown_int4 == 1).map(|d| (d.dest_or_source_uuid.clone(), d.unknown_big_int7, 0)).collect();
        for d in &dd.unknown_items3 { if let Some(p) = primaries.iter_mut().find(|p| p.0 == d.dest_or_source_uuid) { p.2 = d.unknown_timestamp_microseconds; } }
        if primaries.is_empty() { bail!("no phone found on this Google account — is Messages signed in on the phone?"); }
        primaries.sort_by(|a, b| b.2.cmp(&a.2));
        if primaries.len() > 1 { tracing::warn!(?primaries, "multiple primary devices; using most recently seen"); }
        let (reg_id, unknown_int, _) = primaries.remove(0);
        tracing::info!(%reg_id, n_primaries = primaries.len() + 1, "pairing target");
        self.auth.lock().unwrap().dest_reg_id = Some(reg_id);
        // Open the stream (unauthenticated mode) so the phone's replies have somewhere to land.
        let notified = self.stream_opened.notified();
        let me = self.clone();
        tokio::spawn(async move { me.long_poll_loop_pub(false).await; });
        tokio::time::timeout(Duration::from_secs(30), notified).await.map_err(|_| anyhow!("stream did not open"))?;

        let mut ps = PairingSession::new(unknown_int);
        let init = ps.init_payload.clone();
        let server_init = match tokio::time::timeout(Duration::from_secs(20), self.gaia_message(&ps, ActionType::CreateGaiaPairingClientInit, init)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => { let _ = self.cancel_gaia(&ps).await; return Err(e.context("CLIENT_INIT")); }
            Err(_) => { let _ = self.cancel_gaia(&ps).await; bail!("the phone did not answer the pairing request in 20 s — make sure it's online and Messages is open"); }
        };
        tracing::debug!(kdv = server_init.confirmed_key_derivation_version, vcv = server_init.confirmed_verification_code_version, "server init");
        let emoji = match ps.process_server_init(server_init) { Ok(e) => e, Err(e) => { let _ = self.cancel_gaia(&ps).await; return Err(e); } };
        Ok((emoji, ps))
    }

    /// Phase 2: send CLIENT_FINISH and wait for the user to confirm the emoji on the phone.
    pub async fn finish_gaia_pairing(self: &Arc<Self>, ps: PairingSession) -> Result<String> {
        let resp = self.gaia_message(&ps, ActionType::CreateGaiaPairingClientFinished, ps.finish_payload.clone()).await.context("CLIENT_FINISH")?;
        if resp.finish_error_type != 0 {
            use GaiaPairingErrorCode as E;
            let code = E::try_from(resp.finish_error_code).unwrap_or(E::Unknown);
            bail!(match code {
                E::WrongVerificationCodeSelected => "wrong emoji was chosen on the phone".to_string(),
                E::UserCanceledVerification => "pairing was cancelled on the phone".to_string(),
                E::UserDeniedVerificationNotMe => "pairing was denied on the phone ('this is not me')".to_string(),
                E::RequestOutOfDate | E::RequestNotReceivedQuickly | E::VerificationTimedOut => "pairing timed out".to_string(),
                E::ClientAttestationMissing | E::ClientAttestationMismatch | E::ClientAttestationRevisionMismatch =>
                    format!("the phone rejected Bubo's pairing attestation ({code:?}) — the Messages app probably changed the protocol again; Bubo needs an update"),
                other => format!("pairing failed: {other:?} ({}/{})", resp.finish_error_type, resp.finish_error_code),
            });
        }
        let (aes, hmac) = ps.derive_keys()?;
        let phone = {
            let mut a = self.auth.lock().unwrap();
            a.request_crypto.aes_key = aes; a.request_crypto.hmac_key = hmac;
            a.pairing_id = Some(ps.id.clone());
            format!("{}/{}", a.mobile.as_ref().map(|m| m.source_id.clone()).unwrap_or_default(), ps.dest_unknown_int)
        };
        self.save_auth();
        self.emit(Event::Paired { phone_id: phone.clone() });
        let me = self.clone();
        tokio::spawn(async move { if let Err(e) = me.connect().await { tracing::error!("connect after Gaia pairing: {e:#}"); } });
        Ok(phone)
    }

    async fn gaia_message(&self, ps: &PairingSession, action: ActionType, data: Vec<u8>) -> Result<GaiaPairingResponseContainer> {
        let finish = action == ActionType::CreateGaiaPairingClientFinished;
        let req = GaiaPairingRequestContainer {
            pairing_attempt_id: ps.id.clone(), browser_details: Some(browser_details()), start_timestamp: ps.start_ms, data,
            proposed_verification_code_version: if finish { 0 } else { 1 }, proposed_key_derivation_version: if finish { 0 } else { 1 },
            // Since late Aug 2026 the phone rejects CLIENT_FINISHED with CLIENT_ATTESTATION_MISSING unless this exact string is present.
            private_api_confirmation: if finish { PRIVATE_API_CONFIRMATION.into() } else { String::new() },
        };
        let opts = SendOpts { dont_encrypt: true, custom_ttl: Some(300_000_000), message_type: if finish { MessageType::BugleMessage } else { MessageType::Gaia2 },
            timeout: Duration::from_secs(if finish { 330 } else { 25 }), ..Default::default() };
        let inc = self.send_rpc(action, Some(req), opts).await?.ok_or_else(|| anyhow!("no response"))?;
        let raw = inc.message.as_ref().map(|m| m.unencrypted_data.clone()).unwrap_or_default();
        Ok(GaiaPairingResponseContainer::decode(raw.as_slice())?)
    }

    async fn cancel_gaia(&self, ps: &PairingSession) -> Result<()> {
        self.send_rpc(ActionType::CancelGaiaPairing, None::<crate::gm::proto::util::EmptyArr>, SendOpts { request_id: Some(ps.id.clone()), dont_encrypt: true, custom_ttl: Some(300_000_000), message_type: MessageType::Gaia2, expect_response: false, ..Default::default() }).await.map(|_| ())
    }

    pub(crate) async fn unpair_gaia(&self) -> Result<()> {
        let id = self.auth.lock().unwrap().pairing_id.clone().unwrap_or_default();
        self.send_rpc(ActionType::UnpairGaiaPairing, Some(RevokeGaiaPairingRequest { pairing_attempt_id: id }), SendOpts { expect_response: false, ..Default::default() }).await.map(|_| ())
    }
}
