//! Conversation list + message thread + composer.
use super::state::{Conv, Media, Msg, fmt_time};
use crate::gm::client::Client;
use crate::gm::events::Event;
use crate::gm::proto::client::list_conversations_request::Folder;
use adw::prelude::*;
use gtk4::glib;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

#[derive(Default)]
struct State {
    convs: Vec<Conv>,
    messages: HashMap<String, Vec<Msg>>,
    current: Option<String>,
    rows: HashMap<String, gtk4::ListBoxRow>,
    /// Pagination cursor for older messages per conversation. Absent key = never loaded;
    /// `None` = history exhausted.
    cursors: HashMap<String, Option<crate::gm::proto::client::Cursor>>,
    loading_older: std::collections::HashSet<String>,
}

const PAGE: i64 = 50;

/// Where the thread scroller should settle after its contents change.
#[derive(Clone, Copy, PartialEq, Debug)]
enum ScrollTarget {
    /// The user has scrolled on their own; leave the position alone.
    Free,
    /// Stick to the newest message.
    Bottom,
    /// Keep this distance (in pixels) from the end, so prepended pages don't shift the view.
    FromBottom(f64),
}

pub struct ChatsView {
    pub widget: adw::NavigationSplitView,
    win: adw::ApplicationWindow,
    client: Arc<Client>,
    events: async_channel::Receiver<Event>,
    st: Rc<RefCell<State>>,
    list: gtk4::ListBox,
    thread: gtk4::ListBox,
    thread_scroll: gtk4::ScrolledWindow,
    /// Where the thread should sit once GTK has laid out the rows just added to it.
    scroll_target: Cell<ScrollTarget>,
    /// Full-resolution media bytes keyed by attachment id, so re-rendering a thread never refetches.
    media_cache: Rc<RefCell<HashMap<String, Rc<Vec<u8>>>>>,
    /// Contact photos keyed by participant id. `None` records a participant the phone has no
    /// photo for, so we don't ask again every reload.
    avatars: Rc<RefCell<HashMap<String, Option<gtk4::gdk::Texture>>>>,
    thread_title: adw::WindowTitle,
    entry: gtk4::Entry,
    send: gtk4::Button,
    attach: gtk4::Button,
    toast: adw::ToastOverlay,
    banner: adw::Banner,
    side_stack: gtk4::Stack,
    content_stack: gtk4::Stack,
    composer: gtk4::Box,
    settings: Rc<RefCell<crate::settings::Settings>>,
    notifier: Option<Rc<crate::notify::Notifier>>,
    /// The "+" in the sidebar header that opens the new-conversation picker.
    new_chat: gtk4::Button,
    /// The phone's address book, fetched on first use of the picker and kept for the session.
    contacts: Rc<RefCell<Option<Rc<Vec<ContactEntry>>>>>,
}

/// One address-book entry as the picker shows it.
#[derive(Clone, Debug)]
struct ContactEntry {
    name: String,
    /// The dialable number, as the phone reports it (usually E.164).
    number: String,
    /// Pretty form for display, falling back to `number`.
    formatted: String,
    participant_id: String,
}

/// Keep only what a dialler would: a leading `+` and digits. `None` if the text doesn't look
/// like a phone number at all (letters, or fewer than three digits).
fn normalise_number(input: &str) -> Option<String> {
    let t = input.trim();
    if t.is_empty() || !t.chars().all(|c| c.is_ascii_digit() || "+ ()-.".contains(c)) { return None; }
    let digits: String = t.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 3 { return None; }
    Some(if t.starts_with('+') { format!("+{digits}") } else { digits })
}

/// A centred spinner with a caption, used while a pane is waiting on the phone.
fn loading_page(text: &str) -> gtk4::Widget {
    let spinner = adw::Spinner::new();
    spinner.set_size_request(32, 32);
    let label = gtk4::Label::builder().label(text).css_classes(["dim-label"]).build();
    let col = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(12).halign(gtk4::Align::Center).valign(gtk4::Align::Center).vexpand(true).hexpand(true).build();
    col.append(&spinner); col.append(&label);
    col.upcast()
}

/// Build a stack of named pages, crossfading between them.
fn stack(pages: &[(&str, &gtk4::Widget)]) -> gtk4::Stack {
    let st = gtk4::Stack::builder().transition_type(gtk4::StackTransitionType::Crossfade).transition_duration(150).vexpand(true).build();
    for (name, w) in pages { st.add_named(*w, Some(name)); }
    st
}

impl ChatsView {
    pub fn new(win: &adw::ApplicationWindow, client: Arc<Client>, events: async_channel::Receiver<Event>) -> Self {
        // ── sidebar ──
        let list = gtk4::ListBox::builder().selection_mode(gtk4::SelectionMode::Single).css_classes(["navigation-sidebar", "bubo-convs"]).build();
        let side_scroll = gtk4::ScrolledWindow::builder().child(&list).hscrollbar_policy(gtk4::PolicyType::Never).vexpand(true).build();
        let side_header = adw::HeaderBar::builder().title_widget(&adw::WindowTitle::new("Bubo", "")).build();
        let menu = gtk4::gio::Menu::new();
        menu.append(Some("Preferences"), Some("app.preferences"));
        menu.append(Some("Unpair phone"), Some("app.unpair"));
        side_header.pack_end(&gtk4::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).build());
        let new_chat = gtk4::Button::builder().icon_name("list-add-symbolic").tooltip_text("New conversation").build();
        side_header.pack_start(&new_chat);
        let side_empty = adw::StatusPage::builder().icon_name("chat-message-new-symbolic").title("No conversations").description("Messages from your phone will show up here.").build();
        let side_stack = stack(&[("loading", &loading_page("Loading conversations…")), ("empty", side_empty.upcast_ref()), ("list", side_scroll.upcast_ref())]);
        let side = adw::ToolbarView::new();
        side.add_top_bar(&side_header);
        side.set_content(Some(&side_stack));
        let sidebar = adw::NavigationPage::builder().title("Chats").child(&side).build();

        // ── thread ──
        let thread = gtk4::ListBox::builder().selection_mode(gtk4::SelectionMode::None).css_classes(["boxed-list-separate"]).margin_start(12).margin_end(12).margin_top(8).margin_bottom(8).valign(gtk4::Align::End).build();
        thread.add_css_class("bubo-thread");
        let thread_scroll = gtk4::ScrolledWindow::builder().child(&thread).hscrollbar_policy(gtk4::PolicyType::Never).vexpand(true).build();
        let thread_title = adw::WindowTitle::new("", "");
        let entry = gtk4::Entry::builder().placeholder_text("Message").hexpand(true).build();
        let send = gtk4::Button::builder().icon_name("mail-send-symbolic").css_classes(["suggested-action", "circular"]).build();
        let attach = gtk4::Button::builder().icon_name("mail-attachment-symbolic").css_classes(["circular"]).tooltip_text("Attach a file").build();
        let composer = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(6).margin_start(12).margin_end(12).margin_top(6).margin_bottom(12).build();
        composer.append(&attach); composer.append(&entry); composer.append(&send);
        composer.set_visible(false);
        let banner = adw::Banner::builder().revealed(false).build();
        let content_empty = adw::StatusPage::builder().icon_name("user-available-symbolic").title("Select a conversation").description("Pick a chat from the list to start messaging.").build();
        let content_stack = stack(&[("empty", content_empty.upcast_ref()), ("loading", &loading_page("Loading messages…")), ("thread", thread_scroll.upcast_ref())]);
        let content_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        content_box.append(&banner); content_box.append(&content_stack); content_box.append(&composer);
        let content = adw::ToolbarView::new();
        content.add_top_bar(&adw::HeaderBar::builder().title_widget(&thread_title).build());
        content.set_content(Some(&content_box));
        let toast = adw::ToastOverlay::new(); toast.set_child(Some(&content));
        let content_page = adw::NavigationPage::builder().title("Conversation").child(&toast).build();

        let widget = adw::NavigationSplitView::builder().sidebar(&sidebar).content(&content_page).min_sidebar_width(260.0).max_sidebar_width(360.0).build();

        let css = gtk4::CssProvider::new();
        css.load_from_string("
            .bubo-bubble { padding: 8px 12px; border-radius: 16px; }
            .bubo-me { background: var(--accent-bg-color); color: var(--accent-fg-color); }
            .bubo-them { background: alpha(currentColor, 0.08); }
            .bubo-image { border-radius: 16px; }
            .bubo-thread row { background: transparent; border: none; box-shadow: none; padding: 0; margin: 2px 0; }
            .bubo-meta { font-size: 0.8em; opacity: 0.7; }
            .bubo-snippet { opacity: 0.7; }
            .bubo-convs row { padding: 10px 10px; }
            .bubo-badge { background: var(--accent-bg-color); color: var(--accent-fg-color); border-radius: 999px;
                          min-width: 8px; min-height: 8px; padding: 2px; font-size: 0.65em; font-weight: bold;
                          border: 2px solid var(--window-bg-color); margin: -2px; }
            .navigation-sidebar row:selected .bubo-badge { border-color: transparent; }
        ");
        gtk4::style_context_add_provider_for_display(&gtk4::gdk::Display::default().unwrap(), &css, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);

        let v = Self { widget, win: win.clone(), client, events, st: Rc::default(), list, thread, thread_scroll, scroll_target: Cell::new(ScrollTarget::Free), media_cache: Rc::default(), avatars: Rc::default(), thread_title, entry, send, attach, toast, banner, side_stack, content_stack, composer,
            settings: Rc::new(RefCell::new(crate::settings::Settings::load())), notifier: crate::notify::Notifier::new(), new_chat, contacts: Rc::default() };
        let unpair = gtk4::gio::SimpleAction::new("unpair", None);
        let c = v.client.clone(); let w = win.clone();
        unpair.connect_activate(move |_, _| {
            let c = c.clone();
            crate::rt::spawn(async move { let _ = c.unpair().await; let _ = std::fs::remove_file(crate::gm::auth::path()); });
            w.close();
        });
        win.application().unwrap().add_action(&unpair);
        v
    }

    pub fn start(self: &Rc<Self>) {
        {
            let me = Rc::downgrade(self);
            self.thread_scroll.vadjustment().connect_value_changed(move |_| { if let Some(me) = me.upgrade() { me.on_thread_scrolled(); } });
            let me = Rc::downgrade(self);
            // Row heights only become known after layout, so the range (`upper`) changes some time
            // after rows are appended. Apply the pending scroll target on every such change.
            self.thread_scroll.vadjustment().connect_changed(move |_| { if let Some(me) = me.upgrade() { me.apply_scroll_target(); } });
        }
        // notification click → focus window and open that conversation
        if let Some(n) = &self.notifier {
            let me = self.clone();
            n.set_on_open(move |id, token| { me.focus_window(token); me.jump_to(id); });
        }
        let prefs = gtk4::gio::SimpleAction::new("preferences", None);
        let me = self.clone();
        prefs.connect_activate(move |_, _| me.show_preferences());
        if let Some(app) = self.win.application() { app.add_action(&prefs); }
        let me = self.clone();
        self.new_chat.connect_clicked(move |_| me.show_new_chat());
        // selection → open thread
        let me = self.clone();
        self.list.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            let id = unsafe { row.data::<String>("conv-id").map(|p| p.as_ref().clone()) }.unwrap_or_default();
            me.open(&id);
        });
        // composer
        let me = self.clone();
        self.entry.connect_activate(move |_| me.send_current());
        let me = self.clone();
        self.send.connect_clicked(move |_| me.send_current());
        let me = self.clone();
        self.attach.connect_clicked(move |_| me.pick_and_send());
        // typing indicator: notify the phone (throttled) while the user types
        let me = self.clone();
        let last = Rc::new(RefCell::new(std::time::Instant::now() - std::time::Duration::from_secs(10)));
        self.entry.connect_changed(move |e| {
            if e.text().is_empty() || last.borrow().elapsed() < std::time::Duration::from_secs(4) { return; }
            *last.borrow_mut() = std::time::Instant::now();
            if let Some(id) = me.st.borrow().current.clone() { let c = me.client.clone(); crate::rt::spawn(async move { let _ = c.set_typing(&id, true).await; }); }
        });
        // initial load + event pump
        self.reload_conversations();
        let me = self.clone();
        glib::spawn_future_local(async move {
            while let Ok(ev) = me.events.recv().await { me.handle(ev); }
        });
    }

    fn reload_conversations(self: &Rc<Self>) {
        let c = self.client.clone();
        let (tx, rx) = async_channel::bounded(1);
        crate::rt::spawn(async move { let _ = tx.send(c.list_conversations(50, Folder::Inbox).await).await; });
        let me = self.clone();
        glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok(Ok(r)) => { for c in &r.conversations { me.upsert_conv(Conv::from_proto(c)); } me.rebuild_list(); me.fetch_avatars(); }
                Ok(Err(e)) => { me.rebuild_list(); me.toast.add_toast(adw::Toast::new(&format!("Could not load chats: {e:#}"))); }
                Err(_) => {}
            }
        });
    }

    fn handle(self: &Rc<Self>, ev: Event) {
        match ev {
            Event::Conversation(c) => { self.upsert_conv(Conv::from_proto(&c)); self.rebuild_list(); self.fetch_avatars(); }
            Event::Message { msg, is_old } => { let m = Msg::from_proto(&msg); self.maybe_notify(&m, is_old); self.push_message(m, is_old); }
            Event::PhoneNotResponding => { self.banner.set_title("Your phone isn't responding — is it online?"); self.banner.set_revealed(true); }
            Event::PhoneRespondingAgain | Event::Connected => self.banner.set_revealed(false),
            Event::ListenError(e) => { self.banner.set_title(&format!("Connection trouble: {e}")); self.banner.set_revealed(true); }
            Event::ListenFatal(e) => { self.banner.set_title(&format!("Disconnected: {e}. Run `bubo unpair` and pair again.")); self.banner.set_revealed(true); }
            Event::Unpaired => { self.banner.set_title("This device was unpaired from the phone."); self.banner.set_revealed(true); let _ = std::fs::remove_file(crate::gm::auth::path()); }
            Event::Typing(t) => {
                let cur = self.st.borrow().current.clone();
                if cur.as_deref() == Some(&t.conversation_id) {
                    self.thread_title.set_subtitle(if t.r#type == 1 { "typing…" } else { "" });
                }
            }
            _ => {}
        }
    }

    fn upsert_conv(&self, c: Conv) {
        let mut st = self.st.borrow_mut();
        match st.convs.iter_mut().find(|x| x.id == c.id) {
            Some(x) => { let n = if c.unread { x.unread_count } else { 0 }; *x = c; x.unread_count = n; }
            None => st.convs.push(c),
        }
        st.convs.sort_by(|a, b| b.ts.cmp(&a.ts));
    }

    fn rebuild_list(&self) {
        let selected = self.st.borrow().current.clone();
        while let Some(r) = self.list.row_at_index(0) { self.list.remove(&r); }
        let convs = self.st.borrow().convs.clone();
        let mut rows = HashMap::new();
        for c in &convs {
            let row = conv_row(c, &self.avatars.borrow());
            self.list.append(&row);
            if Some(&c.id) == selected.as_ref() { self.list.select_row(Some(&row)); }
            rows.insert(c.id.clone(), row);
        }
        self.st.borrow_mut().rows = rows;
        self.side_stack.set_visible_child_name(if convs.is_empty() { "empty" } else { "list" });
    }

    /// Ask the phone for contact photos of every conversation participant we haven't resolved
    /// yet, and refresh the list when they land.
    fn fetch_avatars(self: &Rc<Self>) {
        let ids: Vec<String> = self.st.borrow().convs.iter().flat_map(|c| c.participant_ids.iter().cloned()).collect();
        let me = Rc::downgrade(self);
        self.request_avatars(ids, move || { if let Some(me) = me.upgrade() { me.refresh_avatars_in_place(); } });
    }

    /// Resolve photos for `ids` — from the on-disk cache where possible, otherwise from the phone
    /// in batched RPCs — and call `on_done` once anything new is available. Ids already cached or
    /// in flight are skipped, so calling this repeatedly is cheap.
    fn request_avatars(self: &Rc<Self>, ids: Vec<String>, on_done: impl Fn() + 'static) {
        let mut wanted: Vec<String> = Vec::new();
        {
            let mut cache = self.avatars.borrow_mut();
            for p in ids {
                if p.is_empty() || cache.contains_key(&p) || wanted.contains(&p) { continue; }
                match load_cached_avatar(&p) {
                    Some(tex) => { cache.insert(p, Some(tex)); }
                    None => wanted.push(p),
                }
            }
        }
        // Callers built their rows before the disk hits above were loaded, so always apply them now.
        on_done();
        if wanted.is_empty() { return; }
        let mut pending = self.avatars.borrow_mut();
        for p in &wanted { pending.insert(p.clone(), None); } // mark in-flight; overwritten on reply
        drop(pending);
        let c = self.client.clone();
        let (tx, rx) = async_channel::bounded(1);
        crate::rt::spawn(async move {
            let mut out = Vec::new();
            for chunk in wanted.chunks(40) {
                match c.participant_thumbnails(chunk).await {
                    Ok(r) => out.extend(r.thumbnail.into_iter().map(|t| (t.identifier, t.data.map(|d| d.image_buffer).unwrap_or_default()))),
                    Err(e) => tracing::warn!("participant thumbnails: {e:#}"),
                }
            }
            let _ = tx.send(out).await;
        });
        let me = self.clone();
        glib::spawn_future_local(async move {
            let Ok(thumbs) = rx.recv().await else { return };
            let mut any = false;
            for (id, bytes) in thumbs {
                if bytes.is_empty() { continue; }
                tracing::debug!("avatar {id}: {} bytes, head {:02x?}", bytes.len(), &bytes[..bytes.len().min(4)]);
                match gtk4::gdk::Texture::from_bytes(&glib::Bytes::from(&bytes)) {
                    Ok(tex) => { store_cached_avatar(&id, &bytes); me.avatars.borrow_mut().insert(id, Some(tex)); any = true; }
                    Err(e) => tracing::warn!("avatar {id}: undecodable image: {e}"),
                }
            }
            if any { on_done(); }
        });
    }

    /// Swap in resolved photos on existing rows without rebuilding the list (keeps selection/scroll).
    fn refresh_avatars_in_place(&self) {
        let st = self.st.borrow();
        let avatars = self.avatars.borrow();
        for c in &st.convs {
            let Some(row) = st.rows.get(&c.id) else { continue };
            let Some(tex) = c.participant_ids.iter().find_map(|p| avatars.get(p).cloned().flatten()) else { continue };
            if let Some(av) = find_avatar(row.upcast_ref()) { av.set_custom_image(Some(&tex)); }
        }
    }

    fn open(self: &Rc<Self>, id: &str) {
        let conv = self.st.borrow().convs.iter().find(|c| c.id == id).cloned();
        let Some(conv) = conv else { return };
        self.st.borrow_mut().current = Some(id.to_string());
        self.thread_title.set_title(&conv.name);
        self.thread_title.set_subtitle(if conv.is_rcs { "RCS" } else { "SMS/MMS" });
        self.widget.set_show_content(true);
        self.composer.set_visible(true);
        self.entry.grab_focus();
        let loaded = self.st.borrow().messages.contains_key(id);
        if loaded { self.render_thread(ScrollTarget::Bottom); } else {
            while let Some(r) = self.thread.row_at_index(0) { self.thread.remove(&r); }
            self.content_stack.set_visible_child_name("loading");
            let c = self.client.clone(); let id2 = id.to_string();
            let (tx, rx) = async_channel::bounded(1);
            crate::rt::spawn(async move { let _ = tx.send(c.list_messages(&id2, PAGE, None).await).await; });
            let me = self.clone(); let id2 = id.to_string();
            glib::spawn_future_local(async move {
                match rx.recv().await {
                    Ok(Ok(r)) => {
                        let mut msgs: Vec<Msg> = r.messages.iter().map(Msg::from_proto).collect();
                        msgs.sort_by_key(|m| m.ts);
                        let more = (r.messages.len() as i64) >= PAGE;
                        { let mut st = me.st.borrow_mut(); st.messages.insert(id2.clone(), msgs); st.cursors.insert(id2.clone(), r.cursor.filter(|_| more)); }
                        if me.st.borrow().current.as_deref() == Some(&id2) { me.render_thread(ScrollTarget::Bottom); }
                    }
                    Ok(Err(e)) => {
                        if me.st.borrow().current.as_deref() == Some(&id2) { me.content_stack.set_visible_child_name("thread"); }
                        me.toast.add_toast(adw::Toast::new(&format!("Could not load messages: {e:#}")));
                    }
                    Err(_) => {}
                }
            });
        }
        if conv.unread && !conv.latest_message_id.is_empty() {
            let c = self.client.clone(); let (id2, mid) = (id.to_string(), conv.latest_message_id.clone());
            crate::rt::spawn(async move { let _ = c.mark_read(&id2, &mid).await; });
            if let Some(x) = self.st.borrow_mut().convs.iter_mut().find(|c| c.id == id) { x.unread = false; x.unread_count = 0; }
            self.rebuild_list();
        }
    }

    fn push_message(self: &Rc<Self>, m: Msg, is_old: bool) {
        let conv_id = m.conversation_id.clone();
        let viewing = self.st.borrow().current.as_deref() == Some(&conv_id);
        // Decide before touching the list: follow the conversation if the user is at its end or
        // just sent something; otherwise hold their place while they read older messages.
        let adj = self.thread_scroll.vadjustment();
        let target = if m.from_me || self.at_bottom() || self.scroll_target.get() == ScrollTarget::Bottom { ScrollTarget::Bottom } else { ScrollTarget::FromBottom(adj.upper() - adj.value()) };
        let is_new = {
            let mut st = self.st.borrow_mut();
            let list = st.messages.entry(conv_id.clone()).or_default();
            if let Some(x) = list.iter_mut().find(|x| x.id == m.id || (!m.tmp_id.is_empty() && x.tmp_id == m.tmp_id)) { *x = m.clone(); false }
            else { list.push(m.clone()); list.sort_by_key(|m| m.ts); true }
        };
        if is_new && !is_old && !m.from_me && !viewing {
            if let Some(c) = self.st.borrow_mut().convs.iter_mut().find(|c| c.id == conv_id) { c.unread = true; c.unread_count += 1; }
            self.rebuild_list();
        }
        if viewing { self.render_thread(target); }
    }

    fn render_thread(self: &Rc<Self>, target: ScrollTarget) {
        self.scroll_target.set(target);
        while let Some(r) = self.thread.row_at_index(0) { self.thread.remove(&r); }
        let st = self.st.borrow();
        let Some(cur) = &st.current else { return };
        let Some(msgs) = st.messages.get(cur) else { return };
        self.content_stack.set_visible_child_name("thread");
        let group = st.convs.iter().find(|c| &c.id == cur).map(|c| c.is_group).unwrap_or(false);
        if st.cursors.get(cur).map(|c| c.is_some()).unwrap_or(false) {
            let spinner = adw::Spinner::builder().width_request(24).height_request(24).margin_top(8).margin_bottom(8).halign(gtk4::Align::Center).build();
            self.thread.append(&gtk4::ListBoxRow::builder().child(&spinner).activatable(false).selectable(false).build());
        }
        for m in msgs { self.thread.append(&self.bubble(m, group)); }
        drop(st);
        self.apply_scroll_target();
    }

    /// Move the thread to the pending target. Runs after every range change, so the position
    /// holds while rows are laid out and images swap in; a user scroll releases it.
    fn apply_scroll_target(&self) {
        let adj = self.thread_scroll.vadjustment();
        let value = match self.scroll_target.get() {
            ScrollTarget::Free => return,
            ScrollTarget::Bottom => adj.upper() - adj.page_size(),
            ScrollTarget::FromBottom(d) => adj.upper() - d,
        };
        adj.set_value(value.max(0.0));
    }

    /// True when the thread is scrolled to (or within a few lines of) its end.
    fn at_bottom(&self) -> bool {
        let adj = self.thread_scroll.vadjustment();
        adj.upper() - adj.value() - adj.page_size() < 48.0
    }

    /// A value change that leaves the thread where the target says it should be is one of our
    /// own (or GTK clamping after rows were removed); anything else is the user scrolling away.
    fn on_thread_scrolled(self: &Rc<Self>) {
        let adj = self.thread_scroll.vadjustment();
        let holds = match self.scroll_target.get() {
            ScrollTarget::Free => false,
            ScrollTarget::Bottom => self.at_bottom(),
            ScrollTarget::FromBottom(d) => (adj.upper() - adj.value() - d).abs() < 1.0,
        };
        if !holds { self.scroll_target.set(ScrollTarget::Free); }
        self.maybe_load_older();
    }

    /// Called on every scroll: when the top of the thread comes within a screen of view, fetch
    /// the next page of older messages and prepend them without moving what the user is looking at.
    fn maybe_load_older(self: &Rc<Self>) {
        let adj = self.thread_scroll.vadjustment();
        if adj.value() > adj.page_size() { return; }
        let (id, cursor) = {
            let mut st = self.st.borrow_mut();
            let Some(id) = st.current.clone() else { return };
            let Some(Some(cursor)) = st.cursors.get(&id).cloned() else { return };
            if !st.loading_older.insert(id.clone()) { return; }
            (id, cursor)
        };
        let (tx, rx) = async_channel::bounded(1);
        let (c, id2) = (self.client.clone(), id.clone());
        crate::rt::spawn(async move { let _ = tx.send(c.list_messages(&id2, PAGE, Some(cursor)).await).await; });
        let me = self.clone();
        glib::spawn_future_local(async move {
            let r = rx.recv().await;
            me.st.borrow_mut().loading_older.remove(&id);
            match r {
                Ok(Ok(r)) => {
                    let more = (r.messages.len() as i64) >= PAGE;
                    let added = {
                        let mut st = me.st.borrow_mut();
                        st.cursors.insert(id.clone(), r.cursor.filter(|_| more));
                        let list = st.messages.entry(id.clone()).or_default();
                        let before = list.len();
                        for m in r.messages.iter().map(Msg::from_proto) { if !list.iter().any(|x| x.id == m.id) { list.push(m); } }
                        list.sort_by_key(|m| m.ts);
                        list.len() - before
                    };
                    if me.st.borrow().current.as_deref() != Some(&id) { return; }
                    if added == 0 && !more { me.render_thread(ScrollTarget::FromBottom(me.thread_scroll.vadjustment().upper() - me.thread_scroll.vadjustment().value())); return; }
                    // Preserve the visual position: keep the distance from the bottom constant.
                    let adj = me.thread_scroll.vadjustment();
                    let from_bottom = adj.upper() - adj.value();
                    me.render_thread(ScrollTarget::FromBottom(from_bottom));
                    // If the page was short enough that the top is still visible, keep going.
                    let me2 = me.clone();
                    glib::timeout_add_local_once(std::time::Duration::from_millis(100), move || me2.maybe_load_older());
                }
                Ok(Err(e)) => me.toast.add_toast(adw::Toast::new(&format!("Could not load older messages: {e:#}"))),
                Err(_) => {}
            }
        });
    }

    fn send_current(self: &Rc<Self>) {
        let text = self.entry.text().to_string();
        if text.trim().is_empty() { return; }
        let conv = { let st = self.st.borrow(); st.current.as_ref().and_then(|id| st.convs.iter().find(|c| &c.id == id).cloned()) };
        let Some(conv) = conv else { return };
        self.entry.set_text("");
        let c = self.client.clone();
        let (tx, rx) = async_channel::bounded(1);
        let (cid, pid, t) = (conv.id.clone(), conv.default_outgoing_id.clone(), text.clone());
        crate::rt::spawn(async move { let _ = tx.send(c.send_text(&cid, &pid, &t, None).await).await; });
        let me = self.clone();
        glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok(Ok(r)) if r.status == 1 => {}
                Ok(Ok(r)) => me.toast.add_toast(adw::Toast::new(&format!("Phone rejected the message (status {})", r.status))),
                Ok(Err(e)) => me.toast.add_toast(adw::Toast::new(&format!("Send failed: {e:#}"))),
                Err(_) => {}
            }
        });
    }
}

impl ChatsView {
    /// Open a file chooser, upload the chosen file, and send it to the open conversation.
    fn pick_and_send(self: &Rc<Self>) {
        let conv = { let st = self.st.borrow(); st.current.as_ref().and_then(|id| st.convs.iter().find(|c| &c.id == id).cloned()) };
        let Some(conv) = conv else { return };
        let dialog = gtk4::FileDialog::builder().title("Send a file").build();
        let me = self.clone();
        let win = me.win.clone();
        dialog.open(Some(&win), None::<&gtk4::gio::Cancellable>, move |res| {
            let Ok(file) = res else { return };
            let Some(path) = file.path() else { return };
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("file").to_string();
            let data = match std::fs::read(&path) { Ok(d) => d, Err(e) => { me.toast.add_toast(adw::Toast::new(&format!("Could not read file: {e}"))); return; } };
            let mime = gtk4::gio::content_type_guess(Some(&name), Some(data.as_slice())).0.to_string();
            me.toast.add_toast(adw::Toast::new(&format!("Sending {name}…")));
            let (tx, rx) = async_channel::bounded(1);
            let (c, cid, pid, caption) = (me.client.clone(), conv.id.clone(), conv.default_outgoing_id.clone(), me.entry.text().to_string());
            crate::rt::spawn(async move {
                let r = match c.upload_media(&data, &name, &mime).await {
                    Ok(media) => c.send_media(&cid, &pid, media, &caption, None).await.map(|_| ()),
                    Err(e) => Err(e),
                };
                let _ = tx.send(r).await;
            });
            me.entry.set_text("");
            let me2 = me.clone();
            glib::spawn_future_local(async move {
                match rx.recv().await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => me2.toast.add_toast(adw::Toast::new(&format!("Send failed: {e:#}"))),
                    Err(_) => {}
                }
            });
        });
    }

    /// Desktop notification for an inbound message, unless it's ours, backfill, or the
    /// conversation is already open in a focused window.
    fn maybe_notify(self: &Rc<Self>, m: &Msg, is_old: bool) {
        if is_old || m.from_me { return; }
        let st = self.st.borrow();
        let focused_here = self.win.is_active() && st.current.as_deref() == Some(&m.conversation_id);
        if focused_here { return; }
        let conv = st.convs.iter().find(|c| c.id == m.conversation_id);
        let mut title = conv.filter(|c| !c.name.is_empty()).map(|c| c.name.clone())
            .or_else(|| (!m.sender.is_empty()).then(|| m.sender.clone()))
            .unwrap_or_else(|| "New message".into());
        // In a group, prefix the sender so you know who spoke.
        let body = match (conv.map(|c| c.is_group).unwrap_or(false), m.text.trim().is_empty()) {
            (_, true) if !m.media.is_empty() => "📎 Attachment".to_string(),
            (true, _) if !m.sender.is_empty() => format!("{}: {}", m.sender, m.text),
            _ => m.text.clone(),
        };
        drop(st);
        let otp = crate::notify::detect_otp(&m.text);
        if let Some(code) = &otp { title = format!("{code} · {title}"); }
        let Some(n) = &self.notifier else { return };
        n.send(crate::notify::Notice { conversation_id: m.conversation_id.clone(), title, body, otp }, &self.settings.borrow().notification_sound);
    }

    /// Bring the window to the front. On Wayland a compositor only grants focus to a window
    /// holding a fresh xdg-activation token, which the notification daemon supplies with the
    /// click; without one, fall back to asking the compositor directly where we know how.
    fn focus_window(&self, token: Option<String>) {
        match token {
            Some(t) => { self.win.set_startup_id(&t); self.win.present(); }
            None => {
                self.win.present();
                let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_lowercase();
                if desktop.contains("hyprland") {
                    let _ = std::process::Command::new("hyprctl").args(["dispatch", "focuswindow", "class:dev.turbinebmw.Bubo"]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
                }
            }
        }
    }

    fn jump_to(self: &Rc<Self>, id: &str) {
        let row = self.st.borrow().rows.get(id).cloned();
        match row { Some(row) => self.list.select_row(Some(&row)), None => self.open(id) }
    }

    /// A picker over the phone's contacts, with a free-text row for numbers not in the book.
    fn show_new_chat(self: &Rc<Self>) {
        let dialog = adw::Dialog::builder().title("New conversation").content_width(400).content_height(560).build();
        let search = gtk4::SearchEntry::builder().placeholder_text("Name or phone number").margin_start(12).margin_end(12).margin_bottom(6).build();
        let list = gtk4::ListBox::builder().selection_mode(gtk4::SelectionMode::None).css_classes(["navigation-sidebar"]).build();
        let scroll = gtk4::ScrolledWindow::builder().child(&list).hscrollbar_policy(gtk4::PolicyType::Never).vexpand(true).build();
        let empty = adw::StatusPage::builder().icon_name("system-search-symbolic").title("No matches").description("Type a phone number to message someone new.").build();
        let pages = stack(&[("loading", &loading_page("Loading contacts…")), ("list", scroll.upcast_ref()), ("empty", empty.upcast_ref())]);
        let col = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        col.append(&search); col.append(&pages);
        let tv = adw::ToolbarView::new();
        tv.add_top_bar(&adw::HeaderBar::new());
        tv.set_content(Some(&col));
        dialog.set_child(Some(&tv));

        // Rebuild the rows for the current query. Each row carries the number to dial.
        let me_w = Rc::downgrade(self);
        let (list2, pages2) = (list.clone(), pages.clone());
        let fill: Rc<dyn Fn(&str)> = Rc::new(move |query: &str| {
            let Some(me) = me_w.upgrade() else { return };
            let Some(contacts) = me.contacts.borrow().clone() else { pages2.set_visible_child_name("loading"); return };
            while let Some(r) = list2.row_at_index(0) { list2.remove(&r); }
            let q = query.trim().to_lowercase();
            let qdigits: String = q.chars().filter(|c| c.is_ascii_digit()).collect();
            let mut n = 0;
            if let Some(num) = normalise_number(query) {
                let row = adw::ActionRow::builder().title(format!("Send to {num}")).subtitle("Not in your contacts").activatable(true).build();
                let icon = gtk4::Image::from_icon_name("phone-symbolic"); icon.set_pixel_size(24);
                row.add_prefix(&icon);
                unsafe { row.set_data("number", num); }
                list2.append(&row); n += 1;
            }
            let avatars = me.avatars.borrow();
            for c in contacts.iter() {
                let hit = q.is_empty() || c.name.to_lowercase().contains(&q)
                    || (!qdigits.is_empty() && c.number.chars().filter(|c| c.is_ascii_digit()).collect::<String>().contains(&qdigits));
                if !hit { continue; }
                let row = adw::ActionRow::builder().title(glib::markup_escape_text(&c.name)).subtitle(glib::markup_escape_text(&c.formatted)).activatable(true).build();
                let av = adw::Avatar::new(32, Some(&c.name), true);
                if let Some(Some(tex)) = avatars.get(&c.participant_id) { av.set_custom_image(Some(tex)); }
                row.add_prefix(&av);
                unsafe { row.set_data("number", c.number.clone()); row.set_data("pid", c.participant_id.clone()); }
                list2.append(&row); n += 1;
            }
            pages2.set_visible_child_name(if n == 0 { "empty" } else { "list" });
        });

        // Photos: fetch any the visible rows lack, and paint them onto those rows as they land
        // (without rebuilding, so the user's place in the list holds).
        let me_w = Rc::downgrade(self);
        let l = list.clone();
        let paint: Rc<dyn Fn()> = Rc::new(move || {
            let Some(me) = me_w.upgrade() else { return };
            let avatars = me.avatars.borrow();
            let mut i = 0;
            while let Some(row) = l.row_at_index(i) {
                i += 1;
                let Some(pid) = (unsafe { row.data::<String>("pid").map(|p| p.as_ref().clone()) }) else { continue };
                let Some(Some(tex)) = avatars.get(&pid) else { continue };
                if let Some(av) = find_avatar(row.upcast_ref()) { av.set_custom_image(Some(tex)); }
            }
        });
        let (me_w, l, p) = (Rc::downgrade(self), list.clone(), paint.clone());
        let fill_inner = fill;
        let fill: Rc<dyn Fn(&str)> = Rc::new(move |q: &str| {
            fill_inner(q);
            let Some(me) = me_w.upgrade() else { return };
            let mut ids = Vec::new();
            let mut i = 0;
            while let Some(row) = l.row_at_index(i) {
                i += 1;
                if let Some(pid) = unsafe { row.data::<String>("pid").map(|p| p.as_ref().clone()) } { ids.push(pid); }
            }
            let p = p.clone();
            me.request_avatars(ids, move || p());
        });

        let f = fill.clone();
        search.connect_search_changed(move |e| f(&e.text()));
        let (me, d) = (self.clone(), dialog.clone());
        list.connect_row_activated(move |_, row| {
            let Some(num) = (unsafe { row.data::<String>("number").map(|p| p.as_ref().clone()) }) else { return };
            d.close();
            me.start_conversation(num);
        });
        // Enter in the search box takes the first row (the typed number, or the best match).
        let l = list.clone();
        search.connect_activate(move |_| { if let Some(row) = l.row_at_index(0) { l.emit_by_name::<()>("row-activated", &[&row]); } });

        fill("");
        if self.contacts.borrow().is_none() {
            let c = self.client.clone();
            let (tx, rx) = async_channel::bounded(1);
            crate::rt::spawn(async move { let _ = tx.send(c.list_contacts().await).await; });
            let (me, f, s, toast) = (self.clone(), fill.clone(), search.clone(), self.toast.clone());
            glib::spawn_future_local(async move {
                match rx.recv().await {
                    Ok(Ok(r)) => {
                        let mut v: Vec<ContactEntry> = r.contacts.into_iter().filter_map(|c| {
                            let n = c.number?;
                            let number = if !n.number.is_empty() { n.number } else { n.number2 };
                            if number.is_empty() { return None; }
                            let formatted = n.formatted_number.filter(|f| !f.is_empty()).unwrap_or_else(|| number.clone());
                            let name = if c.name.is_empty() { formatted.clone() } else { c.name };
                            Some(ContactEntry { name, number, formatted, participant_id: c.participant_id })
                        }).collect();
                        v.sort_by_key(|c| c.name.to_lowercase());
                        *me.contacts.borrow_mut() = Some(Rc::new(v));
                    }
                    Ok(Err(e)) => { *me.contacts.borrow_mut() = Some(Rc::new(Vec::new())); toast.add_toast(adw::Toast::new(&format!("Could not load contacts: {e:#}"))); }
                    Err(_) => return,
                }
                f(&s.text());
            });
        }
        dialog.present(Some(&self.win));
        search.grab_focus();
    }

    /// Ask the phone for the thread with `number` (creating it if needed), then open it.
    fn start_conversation(self: &Rc<Self>, number: String) {
        let c = self.client.clone();
        let (tx, rx) = async_channel::bounded(1);
        let n = number.clone();
        crate::rt::spawn(async move { let _ = tx.send(c.get_or_create_conversation(&[n]).await).await; });
        let me = self.clone();
        glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok(Ok(r)) => match r.conversation {
                    Some(conv) => {
                        let id = conv.conversation_id.clone();
                        me.upsert_conv(Conv::from_proto(&conv));
                        me.rebuild_list(); me.fetch_avatars();
                        me.jump_to(&id);
                    }
                    None => me.toast.add_toast(adw::Toast::new(&format!("Your phone couldn't start a chat with {number}"))),
                },
                Ok(Err(e)) => me.toast.add_toast(adw::Toast::new(&format!("Could not start conversation: {e:#}"))),
                Err(_) => {}
            }
        });
    }

    fn show_preferences(self: &Rc<Self>) {
        use crate::settings::Sound;
        let dialog = adw::PreferencesDialog::new();
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::builder().title("Notifications")
            .description("The sound is requested from your notification daemon, which decides whether to play it — so do-not-disturb rules in your shell still apply.").build();
        let choices = gtk4::StringList::new(&["System default", "Custom file", "None"]);
        let sound_row = adw::ComboRow::builder().title("Sound").model(&choices).build();
        let file_row = adw::ActionRow::builder().title("Sound file").activatable(true).build();
        file_row.add_suffix(&gtk4::Image::from_icon_name("folder-open-symbolic"));
        let current = self.settings.borrow().notification_sound.clone();
        sound_row.set_selected(match &current { Sound::SystemDefault => 0, Sound::File(_) => 1, Sound::None => 2 });
        if let Sound::File(p) = &current { file_row.set_subtitle(&p.to_string_lossy()); }
        file_row.set_visible(matches!(current, Sound::File(_)));
        let (me, fr) = (self.clone(), file_row.clone());
        sound_row.connect_selected_notify(move |r| {
            let mut s = me.settings.borrow_mut();
            s.notification_sound = match r.selected() {
                0 => Sound::SystemDefault,
                1 => match &s.notification_sound { Sound::File(p) => Sound::File(p.clone()), _ => Sound::File(std::path::PathBuf::new()) },
                _ => Sound::None,
            };
            fr.set_visible(r.selected() == 1);
            s.save();
        });
        let (me, win) = (self.clone(), self.win.clone());
        file_row.connect_activated(move |row| {
            let filter = gtk4::FileFilter::new(); filter.set_name(Some("Audio")); filter.add_mime_type("audio/*");
            let filters = gtk4::gio::ListStore::new::<gtk4::FileFilter>(); filters.append(&filter);
            let chooser = gtk4::FileDialog::builder().title("Choose a notification sound").default_filter(&filter).filters(&filters).modal(true).build();
            let (me, row) = (me.clone(), row.clone());
            chooser.open(Some(&win), gtk4::gio::Cancellable::NONE, move |res| {
                if let Ok(f) = res { if let Some(p) = f.path() {
                    row.set_subtitle(&p.to_string_lossy());
                    let mut s = me.settings.borrow_mut(); s.notification_sound = Sound::File(p); s.save();
                } }
            });
        });
        let test = adw::ButtonRow::builder().title("Send a test notification").build();
        let me = self.clone();
        test.connect_activated(move |_| {
            if let Some(n) = &me.notifier {
                n.send(crate::notify::Notice { conversation_id: String::new(), title: "123456 · Bubo".into(), body: "Your verification code is 123456".into(), otp: Some("123456".into()) }, &me.settings.borrow().notification_sound);
            }
        });
        group.add(&sound_row); group.add(&file_row); group.add(&test);
        page.add(&group);
        dialog.add(&page);
        dialog.present(Some(&self.win));
    }
}

/// One conversation in the sidebar, laid out Teams-style: the name and a single-line preview
/// together stand exactly as tall as the avatar, with the time top-right and an unread badge
/// pinned to the avatar's corner (blank for one unread message, a count for more).
fn conv_row(c: &Conv, avatars: &HashMap<String, Option<gtk4::gdk::Texture>>) -> gtk4::ListBoxRow {
    const SIZE: i32 = 40;
    let avatar = adw::Avatar::new(SIZE, Some(&c.name), true);
    if c.is_group { avatar.set_icon_name(Some("system-users-symbolic")); }
    // First participant with a contact photo wins (for groups too — matches the phone's habit).
    if let Some(tex) = c.participant_ids.iter().find_map(|p| avatars.get(p).cloned().flatten()) {
        avatar.set_custom_image(Some(&tex));
    }
    let overlay = gtk4::Overlay::builder().child(&avatar).valign(gtk4::Align::Center).build();
    if c.unread {
        let badge = gtk4::Label::builder().css_classes(["bubo-badge"]).halign(gtk4::Align::End).valign(gtk4::Align::End).build();
        if c.unread_count > 1 { badge.set_label(&c.unread_count.to_string()); }
        overlay.add_overlay(&badge);
    }

    let name = gtk4::Label::builder().label(&c.name).xalign(0.0).ellipsize(gtk4::pango::EllipsizeMode::End).hexpand(true).valign(gtk4::Align::End).build();
    if c.unread { name.add_css_class("heading"); }
    let time = gtk4::Label::builder().label(fmt_time(c.ts)).css_classes(["bubo-meta"]).valign(gtk4::Align::End).build();
    let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 8); top.append(&name); top.append(&time);

    let preview = if c.snippet.is_empty() { String::new() }
        else if c.last_from_me { format!("You: {}", c.snippet) }
        else if c.is_group && !c.last_sender.is_empty() { format!("{}: {}", c.last_sender, c.snippet) }
        else { c.snippet.clone() };
    let snippet = gtk4::Label::builder().label(preview.replace('\n', " ")).xalign(0.0).ellipsize(gtk4::pango::EllipsizeMode::End)
        .single_line_mode(true).valign(gtk4::Align::Start).css_classes(["bubo-snippet", "caption"]).build();

    // Two rows sharing the avatar's height: name sits on the midline, preview hangs below it.
    let col = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).homogeneous(true).hexpand(true).height_request(SIZE).valign(gtk4::Align::Center).build();
    col.append(&top); col.append(&snippet);

    let row_box = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(12).build();
    row_box.append(&overlay); row_box.append(&col);
    let row = gtk4::ListBoxRow::builder().child(&row_box).build();
    unsafe { row.set_data("conv-id", c.id.clone()); }
    row
}

impl ChatsView {
    fn bubble(self: &Rc<Self>, m: &Msg, group: bool) -> gtk4::ListBoxRow {
    let halign = if m.from_me { gtk4::Align::End } else { gtk4::Align::Start };
    let col = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(4).halign(halign).build();
    let bubble = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(6).css_classes(["bubo-bubble", if m.from_me { "bubo-me" } else { "bubo-them" }]).halign(halign).build();
    // Group chats: attribute each message Google-Messages style — a small contact photo and the
    // sender's full name in their avatar colour, sitting above the bubble rather than inside it.
    if group && !m.from_me && !m.sender_full.is_empty() {
        let av = adw::Avatar::new(24, Some(&m.sender_full), true);
        if let Some(tex) = self.avatars.borrow().get(&m.sender_id).cloned().flatten() { av.set_custom_image(Some(&tex)); }
        let name = gtk4::Label::builder().label(&m.sender_full).xalign(0.0).css_classes(["caption", "heading"]).build();
        if m.sender_color.len() == 7 && m.sender_color.starts_with('#') {
            name.set_markup(&format!("<span foreground=\"{}\">{}</span>", m.sender_color, glib::markup_escape_text(&m.sender_full)));
        }
        let hdr = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(6).halign(gtk4::Align::Start).build();
        hdr.append(&av); hdr.append(&name);
        col.append(&hdr);
    }
    // Images stand alone (no bubble, Android-Messages style); other files sit inside the bubble.
    for md in &m.media {
        let w = self.attachment_widget(md);
        if md.is_image() { w.set_halign(halign); col.append(&w); } else { bubble.append(&w); }
    }
    if !m.text.trim().is_empty() {
        bubble.append(&gtk4::Label::builder().label(&m.text).wrap(true).wrap_mode(gtk4::pango::WrapMode::WordChar).xalign(0.0).selectable(true).max_width_chars(60).build());
    }
    if bubble.first_child().is_some() { col.append(&bubble); }
    let mut meta = fmt_time(m.ts);
    if m.from_me { meta.push_str(match m.status { 1 | 2 | 3 | 4 | 5 | 6 => " · sent", 11 => " · delivered", 12 => " · read", s if s >= 100 => " · failed", _ => "" }); }
    let meta = gtk4::Label::builder().label(&meta).css_classes(["bubo-meta"]).halign(halign).build();
    col.append(&meta);
    gtk4::ListBoxRow::builder().child(&col).activatable(false).selectable(false).build()
    }

    /// Fetch the full-resolution image the first time `pic` is within one viewport-height of the
    /// visible area of the thread scroller; then replace the placeholder with it.
    fn lazy_load_image(self: &Rc<Self>, holder: &gtk4::Box, pic: &gtk4::Picture, att_id: String, key: Vec<u8>) {
        let sw = self.thread_scroll.clone();
        let fired = Rc::new(std::cell::Cell::new(false));
        let (me, holder, pic0) = (self.clone(), holder.clone(), pic.clone());
        let check: Rc<dyn Fn()> = Rc::new(move || {
            let pic = &pic0;
            if fired.get() || !pic.is_mapped() { return; }
            let Some(b) = pic.compute_bounds(&sw) else { return };
            let vh = sw.height() as f32;
            if b.y() > vh * 2.0 || b.y() + b.height() < -vh { return; }
            fired.set(true);
            let (tx, rx) = async_channel::bounded(1);
            let (c, id, key) = (me.client.clone(), att_id.clone(), key.clone());
            crate::rt::spawn(async move { let _ = tx.send(c.download_media(&id, &key).await).await; });
            let (me, holder, pic, id) = (me.clone(), holder.clone(), pic.clone(), att_id.clone());
            glib::spawn_future_local(async move {
                match rx.recv().await {
                    Ok(Ok(bytes)) => {
                        let bytes = Rc::new(bytes);
                        me.media_cache.borrow_mut().insert(id, bytes.clone());
                        match Self::image_picture(&bytes) {
                            Ok(full) => { if pic.parent().is_some() { holder.remove(&pic); holder.append(&full); } }
                            Err(e) => tracing::warn!("could not decode image: {e}"),
                        }
                    }
                    Ok(Err(e)) => tracing::warn!("image download failed: {e:#}"),
                    Err(_) => {}
                }
            });
        });
        // Re-check on map, on scroll, and whenever the scroller is resized.
        let c = check.clone(); pic.connect_map(move |_| c());
        let c = check.clone(); let weak = pic.downgrade();
        let id = self.thread_scroll.vadjustment().connect_value_changed(move |_| { if weak.upgrade().is_some() { c(); } });
        let adj = self.thread_scroll.vadjustment();
        let handler = Rc::new(RefCell::new(Some(id)));
        pic.connect_unrealize(move |_| { if let Some(id) = handler.borrow_mut().take() { adj.disconnect(id); } });
        // The scroller lays out after map; poll once shortly after so the initial screen fills in.
        let c = check.clone(); glib::timeout_add_local_once(std::time::Duration::from_millis(50), move || c());
    }

    /// Bare image, scaled to fit within 360x480 (small thumbnails are scaled up), rounded corners.
    /// GIFs animate: frames are pulled from a PixbufAnimation on a timer for as long as the widget lives.
    fn image_picture(bytes: &[u8]) -> anyhow::Result<gtk4::Picture> {
        let pic = gtk4::Picture::new();
        pic.set_content_fit(gtk4::ContentFit::Fill);
        pic.set_can_shrink(true);
        pic.set_overflow(gtk4::Overflow::Hidden);
        pic.add_css_class("bubo-image");
        let fit = |pic: &gtk4::Picture, w: i32, h: i32| {
            let (w, h) = (w.max(1) as f64, h.max(1) as f64);
            let k = (360.0 / w).min(480.0 / h);
            pic.set_size_request((w * k).round() as i32, (h * k).round() as i32);
        };
        if bytes.starts_with(b"GIF8") {
            use gtk4::gdk_pixbuf::{PixbufAnimation, PixbufLoader};
            let loader = PixbufLoader::new();
            loader.write(bytes)?;
            loader.close()?;
            let anim: PixbufAnimation = loader.animation().ok_or_else(|| anyhow::anyhow!("no animation"))?;
            fit(&pic, anim.width(), anim.height());
            let iter = anim.iter(None);
            pic.set_paintable(Some(&gtk4::gdk::Texture::for_pixbuf(&iter.pixbuf())));
            if !anim.is_static_image() {
                fn tick(pic: glib::WeakRef<gtk4::Picture>, iter: gtk4::gdk_pixbuf::PixbufAnimationIter) {
                    let delay = iter.delay_time().unwrap_or(std::time::Duration::from_millis(100)).max(std::time::Duration::from_millis(20));
                    glib::timeout_add_local_once(delay, move || {
                        let Some(p) = pic.upgrade() else { return };
                        iter.advance(std::time::SystemTime::now());
                        p.set_paintable(Some(&gtk4::gdk::Texture::for_pixbuf(&iter.pixbuf())));
                        tick(pic, iter);
                    });
                }
                tick(pic.downgrade(), iter);
            }
        } else {
            let tex = gtk4::gdk::Texture::from_bytes(&glib::Bytes::from(bytes))?;
            fit(&pic, tex.width(), tex.height());
            pic.set_paintable(Some(&tex));
        }
        Ok(pic)
    }

    /// A clickable attachment: images load inline on click; other files save to ~/Downloads.
    fn attachment_widget(self: &Rc<Self>, md: &Media) -> gtk4::Widget {
        let icon = if md.is_image() { "🖼" } else { "📎" };
        let holder = gtk4::Box::new(gtk4::Orientation::Vertical, 4);

        // Inline bytes (no download needed) — render or offer to save immediately.
        if !md.inline.is_empty() {
            if md.is_image() {
                if let Ok(pic) = Self::image_picture(&md.inline) {
                    holder.append(&pic);
                    // Inline bytes are a low-res preview. Show it immediately as a placeholder and
                    // swap in the full image once the widget scrolls into (or near) the viewport.
                    if let Some((att_id, key)) = md.source() {
                        if let Some(bytes) = self.media_cache.borrow().get(&att_id).cloned() {
                            if let Ok(full) = Self::image_picture(&bytes) { holder.remove(&pic); holder.append(&full); }
                            return holder.upcast();
                        }
                        self.lazy_load_image(&holder, &pic, att_id, key);
                    }
                    return holder.upcast();
                }
            }
        }

        let btn = gtk4::Button::builder().label(&format!("{icon} {}", md.label())).css_classes(["flat"]).halign(gtk4::Align::Start).build();
        holder.append(&btn);
        let Some((att_id, key)) = md.source() else {
            btn.set_sensitive(false);
            btn.set_label(&format!("{icon} {} (not available)", md.label()));
            return holder.upcast();
        };
        let (me, md, holder2, btn2) = (self.clone(), md.clone(), holder.clone(), btn.clone());
        btn.connect_clicked(move |_| {
            btn2.set_sensitive(false);
            btn2.set_label(&format!("⏳ {}", md.label()));
            let (tx, rx) = async_channel::bounded(1);
            let (c, id, key) = (me.client.clone(), att_id.clone(), key.clone());
            crate::rt::spawn(async move { let _ = tx.send(c.download_media(&id, &key).await).await; });
            let (me, md, holder2, btn2) = (me.clone(), md.clone(), holder2.clone(), btn2.clone());
            glib::spawn_future_local(async move {
                match rx.recv().await {
                    Ok(Ok(bytes)) => {
                        if md.is_image() {
                            match Self::image_picture(&bytes) {
                                Ok(pic) => {
                                    holder2.remove(&btn2); holder2.append(&pic);
                                }
                                Err(e) => { btn2.set_sensitive(true); btn2.set_label(&format!("🖼 {}", md.label())); me.toast.add_toast(adw::Toast::new(&format!("Could not show image: {e}"))); }
                            }
                        } else {
                            match save_download(&md, &bytes) {
                                Ok(path) => { btn2.set_label(&format!("✓ {}", md.label())); me.toast.add_toast(adw::Toast::new(&format!("Saved to {}", path.display()))); }
                                Err(e) => { btn2.set_sensitive(true); btn2.set_label(&format!("📎 {}", md.label())); me.toast.add_toast(adw::Toast::new(&format!("Save failed: {e}"))); }
                            }
                        }
                    }
                    Ok(Err(e)) => { btn2.set_sensitive(true); btn2.set_label(&format!("{} {}", if md.is_image() { "🖼" } else { "📎" }, md.label())); me.toast.add_toast(adw::Toast::new(&format!("Download failed: {e:#}"))); }
                    Err(_) => {}
                }
            });
        });
        holder.upcast()
    }
}

fn avatar_cache_path(participant_id: &str) -> Option<std::path::PathBuf> {
    // Participant ids are opaque strings; hash them so they're safe filenames.
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    participant_id.hash(&mut h);
    let dir = directories::ProjectDirs::from("dev", "turbinebmw", "bubo")?.cache_dir().join("avatars");
    Some(dir.join(format!("{:016x}", h.finish())))
}

fn load_cached_avatar(participant_id: &str) -> Option<gtk4::gdk::Texture> {
    let bytes = std::fs::read(avatar_cache_path(participant_id)?).ok()?;
    gtk4::gdk::Texture::from_bytes(&glib::Bytes::from(&bytes)).ok()
}

fn store_cached_avatar(participant_id: &str, bytes: &[u8]) {
    let Some(p) = avatar_cache_path(participant_id) else { return };
    if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
    let _ = std::fs::write(p, bytes);
}

/// Depth-first search for the `adw::Avatar` inside a conversation row.
fn find_avatar(w: &gtk4::Widget) -> Option<adw::Avatar> {
    if let Some(a) = w.downcast_ref::<adw::Avatar>() { return Some(a.clone()); }
    let mut child = w.first_child();
    while let Some(c) = child {
        if let Some(a) = find_avatar(&c) { return Some(a); }
        child = c.next_sibling();
    }
    None
}

fn save_download(md: &Media, bytes: &[u8]) -> anyhow::Result<std::path::PathBuf> {
    let dir = directories::UserDirs::new().and_then(|d| d.download_dir().map(|p| p.to_path_buf())).unwrap_or_else(|| std::path::PathBuf::from("."));
    std::fs::create_dir_all(&dir)?;
    let name = if md.name.is_empty() { format!("bubo-{}", &md.id[..md.id.len().min(8)]) } else { md.name.clone() };
    let mut path = dir.join(&name);
    let (stem, ext) = match name.rsplit_once('.') { Some((s, e)) => (s.to_string(), format!(".{e}")), None => (name.clone(), String::new()) };
    let mut n = 1;
    while path.exists() { path = dir.join(format!("{stem} ({n}){ext}")); n += 1; }
    std::fs::write(&path, bytes)?;
    Ok(path)
}
