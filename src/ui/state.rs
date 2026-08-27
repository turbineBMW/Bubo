//! Plain-data view of conversations and messages, kept on the GTK thread.
use crate::gm::proto::conversations::{Conversation, Message, message_info};

#[derive(Clone, Debug)]
pub struct Msg {
    pub id: String,
    pub tmp_id: String,
    pub conversation_id: String,
    pub from_me: bool,
    pub sender: String,
    /// Sender's full display name, participant id and avatar colour (for group-chat attribution).
    pub sender_full: String,
    pub sender_id: String,
    pub sender_color: String,
    pub text: String,
    pub media: Vec<Media>,
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
                Some(message_info::Data::MediaContent(mc)) => media.push(Media {
                    id: mc.media_id.clone(), key: mc.decryption_key.clone(),
                    thumb_id: mc.thumbnail_media_id.clone(), thumb_key: mc.thumbnail_decryption_key.clone(),
                    inline: mc.media_data.clone(), name: mc.media_name.clone(), mime: mc.mime_type.clone(),
                }),
                None => {}
            }
        }
        let sp = m.sender_participant.as_ref();
        Self {
            id: m.message_id.clone(), tmp_id: m.tmp_id.clone(), conversation_id: m.conversation_id.clone(),
            from_me: sp.map(|p| p.is_me).unwrap_or(false),
            sender: sp.map(|p| if !p.first_name.is_empty() { p.first_name.clone() } else if !p.full_name.is_empty() { p.full_name.clone() } else { p.formatted_number.clone() }).unwrap_or_default(),
            sender_full: sp.map(|p| if !p.full_name.is_empty() { p.full_name.clone() } else if !p.first_name.is_empty() { p.first_name.clone() } else { p.formatted_number.clone() }).unwrap_or_default(),
            sender_id: sp.and_then(|p| p.id.as_ref()).map(|i| i.participant_id.clone()).unwrap_or_default(),
            sender_color: sp.map(|p| p.avatar_hex_color.clone()).unwrap_or_default(),
            text: text.join("\n"), media, ts: m.timestamp,
            status: m.message_status.as_ref().map(|s| s.status).unwrap_or(0),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Media {
    pub id: String,
    pub key: Vec<u8>,
    pub thumb_id: String,
    pub thumb_key: Vec<u8>,
    /// Small attachments arrive with their bytes inline instead of a downloadable id.
    pub inline: Vec<u8>,
    pub name: String,
    pub mime: String,
}
impl Media {
    pub fn is_image(&self) -> bool { self.mime.starts_with("image/") }
    pub fn label(&self) -> String { if self.name.is_empty() { self.mime.clone() } else { self.name.clone() } }
    /// The best (attachment_id, key) to download, preferring the full image over the thumbnail.
    pub fn source(&self) -> Option<(String, Vec<u8>)> {
        if !self.id.is_empty() { Some((self.id.clone(), self.key.clone())) }
        else if !self.thumb_id.is_empty() { Some((self.thumb_id.clone(), self.thumb_key.clone())) }
        else { None }
    }
}

#[derive(Clone, Debug)]
pub struct Conv {
    pub id: String,
    pub name: String,
    pub snippet: String,
    /// Whether the latest message was sent by us (drives the "You: " prefix in the list).
    pub last_from_me: bool,
    /// Display name of the latest message's sender, when the phone provides one (group chats).
    pub last_sender: String,
    pub ts: i64,
    pub unread: bool,
    /// Incoming messages seen since the conversation was last opened. Tracked locally — the
    /// phone only reports a boolean — so it starts at 0 for conversations already unread at launch.
    pub unread_count: u32,
    pub is_group: bool,
    pub default_outgoing_id: String,
    pub latest_message_id: String,
    pub is_rcs: bool,
    /// Participant ids other than us — the keys used to fetch contact photos from the phone.
    pub participant_ids: Vec<String>,
    /// The phone reports deleted conversations as updates with `status = DELETED` rather than
    /// dropping them, so the list has to filter them out itself.
    pub deleted: bool,
}

impl Conv {
    pub fn from_proto(c: &Conversation) -> Self {
        let lm = c.latest_message.as_ref();
        Self {
            id: c.conversation_id.clone(), name: c.name.clone(),
            snippet: lm.map(|m| m.display_content.clone()).unwrap_or_default(),
            last_from_me: lm.map(|m| m.from_me != 0).unwrap_or(false),
            last_sender: lm.map(|m| m.display_name.clone()).unwrap_or_default(),
            ts: c.last_message_timestamp, unread: c.unread, unread_count: 0, is_group: c.is_group_chat,
            default_outgoing_id: c.default_outgoing_id.clone(), latest_message_id: c.latest_message_id.clone(),
            is_rcs: c.r#type == 2,
            deleted: c.status == 3,
            participant_ids: {
                // `otherParticipants` is only filled for groups; 1:1 chats list everyone in `participants`.
                let mut ids: Vec<String> = c.participants.iter().filter(|p| !p.is_me)
                    .filter_map(|p| p.id.as_ref().map(|i| i.participant_id.clone())).filter(|s| !s.is_empty()).collect();
                for o in &c.other_participants { if !ids.contains(o) { ids.push(o.clone()); } }
                ids
            },
        }
    }
}

pub fn fmt_time(ts_us: i64) -> String {
    let Ok(dt) = gtk4::glib::DateTime::from_unix_local(ts_us / 1_000_000) else { return String::new() };
    let now = gtk4::glib::DateTime::now_local().unwrap();
    let fmt = if dt.ymd() == now.ymd() { "%H:%M" } else if dt.year() == now.year() { "%-d %b" } else { "%-d %b %Y" };
    dt.format(fmt).map(|s| s.to_string()).unwrap_or_default()
}
