//! The spam filter's side of the chat view: which texts to screen, holding their notifications
//! until the model has answered, acting on the verdict, and the review page in Preferences. The
//! screening itself lives in `crate::spam`.
use super::ChatsView;
use crate::settings::{SpamChecker, SpamFilter};
use crate::spam::{Action, Entry, Judgement, Log, Lookup};
use crate::ui::state::{Conv, Msg};
use adw::prelude::*;
use gtk4::glib;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

/// At most this many `claude` or `codex` processes at once.
const PARALLEL: usize = 2;

pub(super) struct Screen {
    log: RefCell<Log>,
    queue: RefCell<VecDeque<String>>,
    /// Conversations queued or being checked.
    busy: RefCell<HashSet<String>>,
    /// Conversations that got another message while being checked; checked again afterwards.
    again: RefCell<HashSet<String>>,
    running: Cell<usize>,
    /// The newest fresh message per conversation whose notification waits on the verdict.
    held: RefCell<HashMap<String, Msg>>,
    /// Said once per run that the command is missing or failing.
    warned: Cell<bool>,
}

impl Screen {
    pub(super) fn new() -> Self {
        Self { log: RefCell::new(Log::load()), queue: Default::default(), busy: Default::default(), again: Default::default(),
            running: Cell::new(0), held: Default::default(), warned: Cell::new(false) }
    }
}

enum Outcome { Pass, Flag(String), Archived }

fn flag_label(j: Judgement) -> String { if j == Judgement::Spam { "⚠ Likely spam".into() } else { "⚠ Possible spam".into() } }

impl ChatsView {
    fn filter(&self) -> SpamFilter { self.settings.borrow().spam_filter }

    /// Cheap check from what's in memory; `spam::suspect` asks the phone for the rest. A
    /// conversation we haven't heard of yet might be a stranger's, so it counts.
    fn maybe_stranger(&self, conv_id: &str) -> bool {
        if self.spam.log.borrow().trusted.contains(conv_id) { return false; }
        let st = self.st.borrow();
        let Some(c) = st.convs.iter().find(|c| c.id == conv_id) else { return true };
        let wrote = c.last_from_me || st.messages.get(conv_id).is_some_and(|ms| ms.iter().any(|m| m.from_me && !m.is_tombstone()));
        !c.is_group && !wrote && !c.members.iter().any(|m| m.is_contact)
    }

    /// A new message just arrived. True if it is being screened, in which case its notification
    /// waits for the verdict.
    pub(super) fn screen_message(self: &Rc<Self>, m: &Msg) -> bool {
        if self.filter() == SpamFilter::Off || m.from_me || !self.maybe_stranger(&m.conversation_id) { return false; }
        self.spam.held.borrow_mut().insert(m.conversation_id.clone(), m.clone());
        self.enqueue(m.conversation_id.clone());
        true
    }

    /// An unread conversation with a stranger that nobody has judged: it came in while Bubo was
    /// closed.
    pub(super) fn screen_backlog(self: &Rc<Self>, conv_id: &str) {
        if self.filter() == SpamFilter::Off || !self.maybe_stranger(conv_id) { return; }
        let pending = self.st.borrow().convs.iter().find(|c| c.id == conv_id).is_some_and(|c| c.unread && !c.last_from_me
            && self.spam.log.borrow().checked.get(conv_id).is_none_or(|&seen| seen < c.ts));
        if pending { self.enqueue(conv_id.to_string()); }
    }

    fn enqueue(self: &Rc<Self>, id: String) {
        if !self.spam.busy.borrow_mut().insert(id.clone()) {
            // Already queued: the check reads the latest history anyway. Running: go again after.
            if !self.spam.queue.borrow().contains(&id) { self.spam.again.borrow_mut().insert(id); }
            return;
        }
        self.spam.queue.borrow_mut().push_back(id);
        self.pump();
    }

    fn pump(self: &Rc<Self>) {
        while self.spam.running.get() < PARALLEL {
            let Some(id) = self.spam.queue.borrow_mut().pop_front() else { return };
            self.spam.running.set(self.spam.running.get() + 1);
            let me = self.clone();
            glib::spawn_future_local(async move {
                let outcome = me.check(&id).await;
                me.spam.running.set(me.spam.running.get() - 1);
                me.spam.busy.borrow_mut().remove(&id);
                if me.spam.again.borrow_mut().remove(&id) { me.enqueue(id); }
                else {
                    let held = me.spam.held.borrow_mut().remove(&id);
                    match (outcome, held) {
                        (Outcome::Pass, Some(m)) => me.notify(&m, None),
                        (Outcome::Flag(label), Some(m)) => me.notify(&m, Some(&label)),
                        _ => {}
                    }
                }
                me.pump();
            });
        }
    }

    async fn check(self: &Rc<Self>, id: &str) -> Outcome {
        let checker = self.settings.borrow().spam_checker;
        let Some(bin) = crate::spam::find(checker) else {
            self.warn(&format!("Spam filter: the `{}` command isn't installed, so texts aren't being checked.", checker.command()));
            return Outcome::Pass;
        };
        let context = self.settings.borrow().spam_context.clone();
        let seen = self.spam.log.borrow().checked.get(id).copied();
        let (tx, rx) = async_channel::bounded(1);
        let (c, id2) = (self.client.clone(), id.to_string());
        crate::rt::spawn(async move {
            let r = async {
                let s = match crate::spam::suspect(&c, &id2, seen).await? { Lookup::Suspect(s) => s, other => return anyhow::Ok(Err(other)) };
                let v = crate::spam::classify(checker, &bin, &context, &s).await;
                Ok(Ok((s, v)))
            }.await;
            let _ = tx.send(r).await;
        });
        let (s, verdict) = match rx.recv().await {
            Ok(Ok(Ok(found))) => found,
            // A message that came in while the last check ran, which saw it too.
            Ok(Ok(Err(Lookup::Unchanged))) => return self.previous(id),
            Ok(Ok(Err(_))) | Err(_) => return Outcome::Pass,
            Ok(Err(e)) => { tracing::warn!("spam filter: reading conversation {id}: {e:#}"); return Outcome::Pass }
        };
        let v = match verdict {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("spam filter: {e:#}");
                self.warn(&format!("Spam filter: {} couldn't check a text (see ~/.cache/bubo/bubo.log).", checker.label()));
                return Outcome::Pass;
            }
        };
        let name = if s.name.is_empty() { s.number.clone() } else { s.name.clone() };
        tracing::info!("spam filter: {name} → {:?} ({checker:?}): {}", v.judgement, v.reason);
        // Don't pull a conversation out from under someone reading it, or hide a code.
        let reading = self.win.is_active() && self.st.borrow().current.as_deref() == Some(id);
        let archive = v.judgement == Judgement::Spam && self.filter() == SpamFilter::Archive && !reading && !s.has_code;
        let mut action = match v.judgement { Judgement::Legit => Action::Passed, _ if archive => Action::Archived, _ => Action::Flagged };
        if action == Action::Archived && let Err(e) = self.archive(id, &s.latest_message_id).await {
            tracing::warn!("spam filter: archiving {name}: {e:#}");
            action = Action::Flagged;
        }
        let entry = Entry { conversation_id: id.to_string(), name: name.clone(), excerpt: s.excerpt(), judgement: v.judgement, reason: v.reason.clone(),
            checker, date: glib::real_time() / 1_000_000, action };
        self.spam.log.borrow_mut().record(entry, s.last_ts);
        match action {
            Action::Archived => {
                let toast = adw::Toast::builder().title(format!("Archived {name} as spam")).button_label("Undo").timeout(8).build();
                let (me, id) = (self.clone(), id.to_string());
                toast.connect_button_clicked(move |_| me.allow(&id, |_| {}));
                self.toast.add_toast(toast);
                Outcome::Archived
            }
            Action::Flagged => Outcome::Flag(flag_label(v.judgement)),
            _ => Outcome::Pass,
        }
    }

    /// What the last check of a conversation decided, for a recheck that found nothing new.
    fn previous(&self, id: &str) -> Outcome {
        match self.spam.log.borrow().entries.iter().rev().find(|e| e.conversation_id == id) {
            Some(e) if e.action == Action::Archived => Outcome::Archived,
            Some(e) if e.action == Action::Flagged => Outcome::Flag(flag_label(e.judgement)),
            _ => Outcome::Pass,
        }
    }

    fn warn(&self, text: &str) {
        if self.spam.warned.replace(true) { return; }
        self.toast.add_toast(adw::Toast::builder().title(text).timeout(0).build());
    }

    /// Mark read and archive on the phone, then drop it from the list without waiting for the
    /// phone's echo.
    async fn archive(self: &Rc<Self>, id: &str, latest_message_id: &str) -> anyhow::Result<()> {
        let (tx, rx) = async_channel::bounded(1);
        let (c, id2, mid) = (self.client.clone(), id.to_string(), latest_message_id.to_string());
        crate::rt::spawn(async move {
            if !mid.is_empty() && let Err(e) = c.mark_read(&id2, &mid).await { tracing::debug!("mark read: {e:#}"); }
            let _ = tx.send(c.set_archived(&id2, true).await).await;
        });
        rx.recv().await.unwrap_or_else(|_| Err(anyhow::anyhow!("the connection closed")))?;
        self.remove_conv(id);
        self.rebuild_list();
        Ok(())
    }

    /// Undo: bring the conversation back to the inbox, and never screen it again. `done` hears
    /// whether the phone agreed; only then is the log changed.
    fn allow(self: &Rc<Self>, id: &str, done: impl Fn(bool) + 'static) {
        let (tx, rx) = async_channel::bounded(1);
        let (c, id2) = (self.client.clone(), id.to_string());
        crate::rt::spawn(async move {
            let r = async { c.set_archived(&id2, false).await?; c.get_conversation(&id2).await }.await;
            let _ = tx.send(r).await;
        });
        let (me, id) = (self.clone(), id.to_string());
        glib::spawn_future_local(async move {
            let Ok(r) = rx.recv().await else { return done(false) };
            match r {
                Ok(conv) => {
                    me.spam.log.borrow_mut().allow(&id);
                    if let Some(conv) = conv {
                        // The phone can answer with the old status before the change lands.
                        let mut conv = Conv::from_proto(&conv);
                        conv.archived = false;
                        me.upsert_conv(conv); me.rebuild_list(); me.fetch_avatars();
                    }
                    done(true);
                }
                Err(e) => { me.toast.add_toast(adw::Toast::new(&format!("Could not restore the conversation: {e:#}"))); done(false); }
            }
        });
    }

    /// Archive a conversation the filter let through or only flagged. `done` hears whether the
    /// phone agreed; only then is the log changed.
    fn archive_now(self: &Rc<Self>, id: &str, done: impl Fn(bool) + 'static) {
        let mid = self.st.borrow().convs.iter().find(|c| c.id == id).map(|c| c.latest_message_id.clone()).unwrap_or_default();
        let (me, id) = (self.clone(), id.to_string());
        glib::spawn_future_local(async move {
            match me.archive(&id, &mid).await {
                Ok(()) => { me.spam.log.borrow_mut().set_action(&id, Action::Archived); done(true); }
                Err(e) => { me.toast.add_toast(adw::Toast::new(&format!("Could not archive: {e:#}"))); done(false); }
            }
        });
    }

    /// The "Spam filter" group for Preferences.
    pub(super) fn spam_preferences(self: &Rc<Self>, dialog: &adw::PreferencesDialog) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder().title("Spam filter").build();
        let describe = |g: &adw::PreferencesGroup, c: SpamChecker| g.set_description(Some(&match (crate::spam::find(c).is_some(), c) {
            (true, SpamChecker::Claude) => "Texts from numbers that aren't in your contacts, in conversations you've never replied to, are checked by Claude Haiku through the claude command. The number, the messages and any pictures are sent to Anthropic. Texts with a verification code are only ever flagged, never archived.".to_string(),
            (true, SpamChecker::Codex) => "Texts from numbers that aren't in your contacts, in conversations you've never replied to, are checked by OpenAI's GPT-6 Luna through the codex command. The number, the messages and any pictures are sent to OpenAI. Texts with a verification code are only ever flagged, never archived.".to_string(),
            (false, c) => format!("Needs the {} command, which wasn't found on PATH or in ~/.local/bin.", c.command()),
        }));
        describe(&group, self.settings.borrow().spam_checker);
        let modes = gtk4::StringList::new(&["Off", "Flag likely spam", "Archive likely spam"]);
        let mode = adw::ComboRow::builder().title("Texts from unknown numbers").model(&modes)
            .subtitle("Archiving also marks the conversation read").build();
        mode.set_selected(match self.filter() { SpamFilter::Off => 0, SpamFilter::Flag => 1, SpamFilter::Archive => 2 });
        let me = self.clone();
        mode.connect_selected_notify(move |r| {
            let mut s = me.settings.borrow_mut();
            s.spam_filter = match r.selected() { 1 => SpamFilter::Flag, 2 => SpamFilter::Archive, _ => SpamFilter::Off };
            s.save();
        });
        let checkers = gtk4::StringList::new(&["Claude (Haiku)", "Codex (GPT-6 Luna)"]);
        let checker = adw::ComboRow::builder().title("Checked by").model(&checkers)
            .subtitle("Uses the account that command is signed in to").build();
        checker.set_selected(match self.settings.borrow().spam_checker { SpamChecker::Claude => 0, SpamChecker::Codex => 1 });
        // Weak: these rows live inside the group and the dialog, so strong refs would keep both alive.
        let (me, g) = (self.clone(), group.downgrade());
        checker.connect_selected_notify(move |r| {
            let c = if r.selected() == 1 { SpamChecker::Codex } else { SpamChecker::Claude };
            { let mut s = me.settings.borrow_mut(); s.spam_checker = c; s.save(); }
            if let Some(g) = g.upgrade() { describe(&g, c); }
            // A newly picked command gets its own warning if it fails.
            me.spam.warned.set(false);
        });
        let context = adw::EntryRow::builder().title("Who might text you out of the blue").show_apply_button(true)
            .text(self.settings.borrow().spam_context.as_str()).build();
        let me = self.clone();
        context.connect_apply(move |e| { let mut s = me.settings.borrow_mut(); s.spam_context = e.text().to_string(); s.save(); });
        let review = adw::ActionRow::builder().title("Review decisions").activatable(true).build();
        review.add_suffix(&gtk4::Image::from_icon_name("go-next-symbolic"));
        let (me, dlg) = (self.clone(), dialog.downgrade());
        review.connect_activated(move |_| { if let Some(d) = dlg.upgrade() { d.push_subpage(&me.spam_review()); } });
        group.add(&mode); group.add(&checker); group.add(&context); group.add(&review);
        group
    }

    /// Everything the filter decided, newest first, each with a way to undo it.
    fn spam_review(self: &Rc<Self>) -> adw::NavigationPage {
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::new();
        let entries: Vec<Entry> = self.spam.log.borrow().entries.iter().rev().cloned().collect();
        if entries.is_empty() {
            group.set_description(Some("Nothing has been checked yet."));
        }
        for e in entries {
            let verdict = match e.judgement { Judgement::Spam => "Spam", Judgement::Unsure => "Unsure", Judgement::Legit => "Legit" };
            let when = super::fmt_time(e.date * 1_000_000);
            let row = adw::ActionRow::builder().title(&e.name).use_markup(false).subtitle_lines(4)
                .subtitle(format!("{verdict} · {} · {when} — {}\n“{}”", e.checker.label(), e.reason, e.excerpt)).build();
            let button = gtk4::Button::builder().valign(gtk4::Align::Center).build();
            let set = |b: &gtk4::Button, action: Action| match action {
                Action::Archived => { b.set_label("Restore"); b.remove_css_class("destructive-action"); }
                Action::Allowed => { b.set_label("Allowed"); b.set_sensitive(false); }
                _ => { b.set_label("Archive"); b.add_css_class("destructive-action"); }
            };
            set(&button, e.action);
            let me = self.clone();
            let action = Rc::new(Cell::new(e.action));
            // The label changes once the phone has done it; until then the button waits.
            button.connect_clicked(move |b| {
                b.set_sensitive(false);
                let to = if action.get() == Action::Archived { Action::Allowed } else { Action::Archived };
                let (b, action) = (b.downgrade(), action.clone());
                let done = move |ok: bool| {
                    if ok { action.set(to); }
                    if let Some(b) = b.upgrade() { b.set_sensitive(true); set(&b, action.get()); }
                };
                if to == Action::Allowed { me.allow(&e.conversation_id, done); } else { me.archive_now(&e.conversation_id, done); }
            });
            row.add_suffix(&button);
            group.add(&row);
        }
        page.add(&group);
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&page));
        adw::NavigationPage::builder().title("Spam filter decisions").child(&view).build()
    }
}
