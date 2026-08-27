//! Request/response correlation over the long-poll stream, plus the ack queue.
//! Every RPC we POST gets its answer back as an event on the ReceiveMessages stream,
//! matched by `sessionID == our requestID`.
use crate::gm::proto::rpc::{ActionType, BugleRoute, RpcMessageData};
use crate::gm::proto::{authentication, events};
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::oneshot;

/// A decoded inbound frame.
#[derive(Debug, Clone)]
pub struct Incoming {
    pub response_id: String,
    pub route: BugleRoute,
    pub is_old: bool,
    pub pair: Option<events::RpcPairData>,
    pub message: Option<RpcMessageData>,
    /// AES-CTR-decrypted `encryptedData`, when present.
    pub decrypted: Option<Vec<u8>>,
}

impl Incoming {
    pub fn action(&self) -> ActionType {
        self.message.as_ref().and_then(|m| ActionType::try_from(m.action).ok()).unwrap_or(ActionType::Unspecified)
    }
    /// Decode the decrypted payload as `M`.
    pub fn decode<M: prost::Message + Default>(&self) -> anyhow::Result<M> {
        let d = self.decrypted.as_deref().ok_or_else(|| anyhow::anyhow!("response to {:?} had no encrypted payload", self.action()))?;
        Ok(M::decode(d)?)
    }
}

#[derive(Default)]
pub struct Session {
    pub session_id: Mutex<String>,
    waiters: Mutex<HashMap<String, oneshot::Sender<Incoming>>>,
    acks: Mutex<Vec<String>>,
}

impl Session {
    pub fn reset_session_id(&self) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        *self.session_id.lock().unwrap() = id.clone();
        id
    }
    pub fn session_id(&self) -> String { self.session_id.lock().unwrap().clone() }

    pub fn wait_for(&self, request_id: &str) -> oneshot::Receiver<Incoming> {
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().unwrap().insert(request_id.to_string(), tx);
        rx
    }
    pub fn cancel(&self, request_id: &str) { self.waiters.lock().unwrap().remove(request_id); }
    pub fn cancel_all(&self) { self.waiters.lock().unwrap().clear(); }

    /// Route a frame to a waiting request. Returns true if consumed.
    pub fn deliver(&self, msg: &Incoming, google: bool) -> bool {
        let Some(m) = &msg.message else { return false };
        // Google-account sessions get odd unencrypted pre-responses before the real one; skip those.
        let gaia_action = matches!(msg.action(), ActionType::CreateGaiaPairingClientInit | ActionType::CreateGaiaPairingClientFinished);
        if google && !gaia_action && !m.unencrypted_data.is_empty() && m.encrypted_data.is_empty() { return false; }
        let tx = self.waiters.lock().unwrap().remove(&m.session_id);
        match tx { Some(tx) => { let _ = tx.send(msg.clone()); true } None => false }
    }

    pub fn queue_ack(&self, id: &str) {
        let mut a = self.acks.lock().unwrap();
        if !a.iter().any(|x| x == id) { a.push(id.to_string()); }
    }
    pub fn take_acks(&self) -> Vec<String> { std::mem::take(&mut *self.acks.lock().unwrap()) }
    pub fn requeue_acks(&self, mut ids: Vec<String>) {
        let mut a = self.acks.lock().unwrap();
        if ids.len() + a.len() <= 1024 { ids.append(&mut a); *a = ids; }
    }
}

pub fn auth_message(request_id: String, token: &[u8], network: &str) -> authentication::AuthMessage {
    authentication::AuthMessage { request_id, network: network.into(), tachyon_auth_token: token.to_vec(), config_version: Some(crate::gm::auth::config_version()) }
}
