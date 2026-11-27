//! Newer Messages builds (seen November 2026) stopped filling the plain id fields:
//! `Conversation.conversationID` (1) and `Message.messageID` (1) / `conversationID` (7) arrive
//! empty, and the ids only come in the structured fields — `someKindOfGroupID.conversationID`
//! and `anotherMessageID.messageID` / `latestMessageAnotherID.messageID`. The phone still takes
//! those ids in requests, so copy them into the plain fields as everything is decoded and the
//! rest of Bubo keeps keying on one id. Older phones fill both; theirs are left alone.

use crate::gm::proto::conversations::{Conversation, Message};

fn fill(slot: &mut String, from: Option<&String>) {
    if slot.is_empty() { if let Some(v) = from.filter(|v| !v.is_empty()) { slot.clone_from(v); } }
}

pub fn conversation(c: &mut Conversation) {
    fill(&mut c.conversation_id, c.some_kind_of_group_id.as_ref().map(|g| &g.conversation_id));
    fill(&mut c.latest_message_id, c.latest_message_anoher_id.as_ref().map(|a| &a.message_id));
}

pub fn message(m: &mut Message) {
    fill(&mut m.message_id, m.another_message_id.as_ref().map(|a| &a.message_id));
    fill(&mut m.conversation_id, m.some_kind_of_group_id.as_ref().map(|g| &g.conversation_id));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gm::proto::conversations::{AnotherMessageId, SomeKindOfGroupId};

    #[test]
    fn fills_empty_ids_from_structured_fields() {
        let mut c = Conversation {
            some_kind_of_group_id: Some(SomeKindOfGroupId { conversation_id: "57".into(), ..Default::default() }),
            latest_message_anoher_id: Some(AnotherMessageId { message_id: "18220".into(), ..Default::default() }),
            ..Default::default()
        };
        conversation(&mut c);
        assert_eq!((c.conversation_id.as_str(), c.latest_message_id.as_str()), ("57", "18220"));

        let mut m = Message {
            some_kind_of_group_id: Some(SomeKindOfGroupId { conversation_id: "57".into(), ..Default::default() }),
            another_message_id: Some(AnotherMessageId { message_id: "896".into(), ..Default::default() }),
            ..Default::default()
        };
        message(&mut m);
        assert_eq!((m.conversation_id.as_str(), m.message_id.as_str()), ("57", "896"));
    }

    #[test]
    fn keeps_ids_older_phones_send() {
        let mut c = Conversation {
            conversation_id: "607".into(),
            some_kind_of_group_id: Some(SomeKindOfGroupId { conversation_id: "other".into(), ..Default::default() }),
            ..Default::default()
        };
        conversation(&mut c);
        assert_eq!(c.conversation_id, "607");
    }
}
