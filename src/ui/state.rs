//! Plain-data view of conversations and messages, kept on the GTK thread.
use crate::gm::proto::conversations::{Conversation, Message, message_info};

#[derive(Clone, Debug)]
pub struct Msg {
    pub id: String,
    pub tmp_id: String,
    pub conversation_id: String,
    pub from_me: bool,
    pub sender: String,
    pub text: String,
    pub media: Vec<String>,
    /// Microseconds since epoch.
    pub ts: i64,
    pub status: i32,
}

impl Msg {
    pub fn from_proto(m: &Message) -> Self {
        let (mut text, mut media) = (Vec::new(), Vec::new());
        for i in &m.message_info {
            match &i.data {
                Some(message_info::Data::MessageContent(c)) => text.push(c.content.clone()),
                Some(message_info::Data::MediaContent(mc)) => media.push(if mc.media_name.is_empty() { mc.mime_type.clone() } else { mc.media_name.clone() }),
                None => {}
            }
        }
        let sp = m.sender_participant.as_ref();
        Self {
            id: m.message_id.clone(), tmp_id: m.tmp_id.clone(), conversation_id: m.conversation_id.clone(),
            from_me: sp.map(|p| p.is_me).unwrap_or(false),
            sender: sp.map(|p| if !p.first_name.is_empty() { p.first_name.clone() } else if !p.full_name.is_empty() { p.full_name.clone() } else { p.formatted_number.clone() }).unwrap_or_default(),
            text: text.join("\n"), media, ts: m.timestamp,
            status: m.message_status.as_ref().map(|s| s.status).unwrap_or(0),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Conv {
    pub id: String,
    pub name: String,
    pub snippet: String,
    pub ts: i64,
    pub unread: bool,
    pub is_group: bool,
    pub default_outgoing_id: String,
    pub latest_message_id: String,
    pub is_rcs: bool,
}

impl Conv {
    pub fn from_proto(c: &Conversation) -> Self {
        let lm = c.latest_message.as_ref();
        Self {
            id: c.conversation_id.clone(), name: c.name.clone(),
            snippet: lm.map(|m| m.display_content.clone()).unwrap_or_default(),
            ts: c.last_message_timestamp, unread: c.unread, is_group: c.is_group_chat,
            default_outgoing_id: c.default_outgoing_id.clone(), latest_message_id: c.latest_message_id.clone(),
            is_rcs: c.r#type == 2,
        }
    }
}

pub fn fmt_time(ts_us: i64) -> String {
    let Ok(dt) = gtk4::glib::DateTime::from_unix_local(ts_us / 1_000_000) else { return String::new() };
    let now = gtk4::glib::DateTime::now_local().unwrap();
    let fmt = if dt.ymd() == now.ymd() { "%H:%M" } else if dt.year() == now.year() { "%-d %b" } else { "%-d %b %Y" };
    dt.format(fmt).map(|s| s.to_string()).unwrap_or_default()
}
