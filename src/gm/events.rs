//! Events the client pushes to the app.
use crate::gm::proto::{conversations, events, settings};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum Event {
    /// QR was scanned and the phone accepted us.
    Paired { phone_id: String },
    /// Pairing revoked on the phone; auth is dead.
    Unpaired,
    Connected,
    /// Transient long-poll failure (will retry).
    ListenError(String),
    /// Fatal (401/403): needs re-pairing.
    ListenFatal(String),
    PhoneNotResponding,
    PhoneRespondingAgain,
    Conversation(conversations::Conversation),
    Message { msg: conversations::Message, is_old: bool },
    Typing(events::TypingData),
    Alert(events::AlertType),
    Settings(settings::Settings),
}
