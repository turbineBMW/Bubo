//! Conversation list + message thread + composer.
use super::state::{Conv, Media, Msg, Reaction, fmt_time};
use crate::gm::client::Client;
use crate::gm::events::Event;
use crate::gm::proto::client::list_conversations_request::Folder;
use crate::gm::proto::client::GetThumbnailResponse;
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

/// The row of one-tap reactions in a message's menu, as in the Messages app; the "+" after it
/// opens the full emoji chooser.
const QUICK_REACTIONS: &[&str] = &["👍", "❤️", "😂", "😮", "😢", "😡"];

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
    /// Set by the app: tears this view down and re-runs the emoji pairing.
    on_session_expired: RefCell<Option<Box<dyn Fn()>>>,
    st: Rc<RefCell<State>>,
    list: gtk4::ListBox,
    thread: gtk4::ListBox,
    thread_scroll: gtk4::ScrolledWindow,
    /// Where the thread should sit once GTK has laid out the rows just added to it.
    scroll_target: Cell<ScrollTarget>,
    /// A frame callback is queued to apply `scroll_target`.
    scroll_queued: Cell<bool>,
    /// Full-resolution media bytes keyed by attachment id, so re-rendering a thread never refetches.
    media_cache: Rc<RefCell<HashMap<String, Rc<Vec<u8>>>>>,
    /// Contact photos keyed by participant id. `None` records a participant the phone has no
    /// photo for, so we don't ask again every reload.
    avatars: Rc<RefCell<HashMap<String, Option<gtk4::gdk::Texture>>>>,
    thread_title: adw::WindowTitle,
    entry: sourceview5::View,
    /// Spell checking for the composer's buffer: underlines, plus suggestions in its context menu.
    spelling: libspelling::TextBufferAdapter,
    emoji_btn: gtk4::Button,
    send: gtk4::Button,
    attach: gtk4::Button,
    gif_btn: gtk4::Button,
    toast: adw::ToastOverlay,
    banner: adw::Banner,
    side_stack: gtk4::Stack,
    content_stack: gtk4::Stack,
    composer: gtk4::Box,
    /// Tray above the composer bar showing staged attachments; hidden while empty.
    pending_box: gtk4::Box,
    /// Attachments staged by pasting, sent with the next message.
    pending: RefCell<Vec<Pending>>,
    settings: Rc<RefCell<crate::settings::Settings>>,
    notifier: Option<Rc<crate::notify::Notifier>>,
    /// The "+" in the sidebar header that opens the new-conversation picker.
    new_chat: gtk4::Button,
    /// The phone's address book, fetched on first use of the picker and kept for the session.
    contacts: Rc<RefCell<Option<Rc<Vec<ContactEntry>>>>>,
}

/// A file waiting in the composer tray until the next send.
struct Pending {
    data: Vec<u8>,
    name: String,
    mime: String,
    tile: gtk4::Widget,
}

/// Clipboard image types read verbatim, so a pasted GIF stays animated and a JPEG isn't
/// re-encoded; any other image the clipboard offers goes through a texture and becomes PNG.
const PASTE_IMAGE_MIMES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// One address-book entry as the picker shows it.
#[derive(Clone, Debug)]
struct ContactEntry {
    name: String,
    /// The dialable number, as the phone reports it (usually E.164).
    number: String,
    /// Pretty form for display, falling back to `number`.
    formatted: String,
    participant_id: String,
    contact_id: String,
}

/// Cache key for an address-book photo. Contact ids live in a different namespace from
/// participant ids, so prefix them to keep the two apart in the avatar cache.
fn contact_key(contact_id: &str) -> String { format!("contact:{contact_id}") }

/// The emoji we reacted to a message with, if any.
fn my_reaction<'a>(reactions: &'a [Reaction], self_ids: &[String]) -> Option<&'a str> {
    reactions.iter().find(|r| r.participant_ids.iter().any(|p| self_ids.contains(p))).map(|r| r.emoji.as_str())
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
        // The thread manages its own scroll position (see `ScrollTarget`). Left on, the viewport
        // and the list both scroll to the focused row, so tearing down rows that hold focus (a
        // message just reacted to, or one with selected text) flung the view to the top.
        let viewport = gtk4::Viewport::builder().child(&thread).scroll_to_focus(false).build();
        let thread_scroll = gtk4::ScrolledWindow::builder().child(&viewport).hscrollbar_policy(gtk4::PolicyType::Never).vexpand(true).build();
        thread.set_adjustment(None::<&gtk4::Adjustment>);
        let thread_title = adw::WindowTitle::new("", "");
        // Multi-line composer: Enter sends, Shift+Enter inserts a newline. The text view grows with
        // its content up to a cap, then scrolls; the buttons sit at the bottom edge either way.
        // A GtkSourceView only so libspelling can check it; without a style scheme it draws
        // like a plain text view and leaves the frame's colours alone.
        let buffer = sourceview5::Buffer::builder().highlight_syntax(false).build();
        sourceview5::prelude::BufferExt::set_style_scheme(&buffer, None);
        let entry = sourceview5::View::builder().buffer(&buffer).wrap_mode(gtk4::WrapMode::WordChar).hexpand(true).accepts_tab(false)
            .top_margin(7).bottom_margin(7).left_margin(10).right_margin(10).css_classes(["bubo-entry"]).build();
        let settings = Rc::new(RefCell::new(crate::settings::Settings::load()));
        // Enchant picks the dictionary from the locale; with none installed nothing is underlined.
        let spelling = libspelling::TextBufferAdapter::new(&buffer, &libspelling::Checker::default());
        entry.set_extra_menu(Some(&spelling.menu_model()));
        entry.insert_action_group("spelling", Some(&spelling));
        spelling.set_enabled(settings.borrow().spell_check);
        let placeholder = gtk4::Label::builder().label("Message").halign(gtk4::Align::Start).valign(gtk4::Align::Start)
            .margin_start(10).margin_top(7).can_target(false).css_classes(["dim-label"]).build();
        let overlay = gtk4::Overlay::builder().child(&entry).build();
        overlay.add_overlay(&placeholder);
        let entry_scroll = gtk4::ScrolledWindow::builder().child(&overlay).hscrollbar_policy(gtk4::PolicyType::Never)
            .propagate_natural_height(true).max_content_height(160).hexpand(true).css_classes(["bubo-entry-frame"]).build();
        let pl = placeholder.clone();
        entry.buffer().connect_changed(move |b| pl.set_visible(b.char_count() == 0));
        let emoji_btn = gtk4::Button::builder().icon_name("emoji-people-symbolic").css_classes(["circular"]).tooltip_text("Insert emoji").valign(gtk4::Align::End).build();
        let send = gtk4::Button::builder().icon_name("mail-send-symbolic").css_classes(["suggested-action", "circular"]).valign(gtk4::Align::End).tooltip_text("Send (Enter)").build();
        let attach = gtk4::Button::builder().icon_name("mail-attachment-symbolic").css_classes(["circular"]).tooltip_text("Attach a file").valign(gtk4::Align::End).build();
        let gif_btn = gtk4::Button::builder().label("GIF").css_classes(["circular", "bubo-gif-btn"]).tooltip_text("Send a GIF").valign(gtk4::Align::End).build();
        let bar = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(6).build();
        bar.append(&attach); bar.append(&gif_btn); bar.append(&emoji_btn); bar.append(&entry_scroll); bar.append(&send);
        let pending_box = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(6).visible(false).build();
        let composer = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(6).margin_start(12).margin_end(12).margin_top(6).margin_bottom(12).valign(gtk4::Align::End).build();
        composer.append(&pending_box); composer.append(&bar);
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
            .bubo-entry-frame { border-radius: 18px; border: 1px solid color-mix(in srgb, currentColor var(--border-opacity), transparent); background: alpha(currentColor, 0.05); }
            .bubo-entry-frame:focus-within { border-color: var(--accent-bg-color); }
            .bubo-entry, .bubo-entry text { background: transparent; }
            /* the scrolled window otherwise reserves the scrollbar slider's 40px minimum, so an empty composer would start two lines tall */
            .bubo-entry-frame scrollbar, .bubo-entry-frame scrollbar slider { min-height: 0; }
            .bubo-gif-btn { font-size: 0.7em; font-weight: bold; padding: 0 6px; }
            .bubo-gif-tile { padding: 0; border-radius: 8px; }
            .bubo-gif-tile picture { border-radius: 8px; }
            .bubo-pending-tile { border-radius: 10px; }
            .bubo-pending-remove { margin: 4px; min-width: 22px; min-height: 22px; padding: 0; }
            .bubo-thread row { background: transparent; border: none; box-shadow: none; padding: 0; margin: 2px 0; }
            .bubo-meta { font-size: 0.8em; opacity: 0.7; }
            /* reaction chips straddle the bubble's bottom edge, ringed in the window colour */
            .bubo-reactions { margin: -10px 10px 0 10px; }
            .bubo-reaction { min-height: 0; min-width: 0; padding: 1px 7px; border-radius: 999px; font-size: 0.85em;
                             background: color-mix(in srgb, currentColor 10%, var(--window-bg-color)); border: 2px solid var(--window-bg-color); }
            .bubo-reaction-mine { background: color-mix(in srgb, var(--accent-bg-color) 35%, var(--window-bg-color)); }
            .bubo-react-pick { font-size: 1.4em; min-width: 40px; min-height: 40px; padding: 0; }
            .bubo-react-pick.bubo-reaction-mine { background: alpha(var(--accent-bg-color), 0.35); }
            .bubo-snippet { opacity: 0.7; }
            .bubo-convs row { padding: 10px 10px; }
            .bubo-badge { background: var(--accent-bg-color); color: var(--accent-fg-color); border-radius: 999px;
                          min-width: 8px; min-height: 8px; padding: 2px; font-size: 0.65em; font-weight: bold;
                          border: 2px solid var(--window-bg-color); margin: -2px; }
            .navigation-sidebar row:selected .bubo-badge { border-color: transparent; }
        ");
        gtk4::style_context_add_provider_for_display(&gtk4::gdk::Display::default().unwrap(), &css, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);

        let v = Self { widget, win: win.clone(), client, events, on_session_expired: RefCell::new(None), st: Rc::default(), list, thread, thread_scroll, scroll_target: Cell::new(ScrollTarget::Free), scroll_queued: Cell::new(false), media_cache: Rc::default(), avatars: Rc::default(), thread_title, entry, spelling, emoji_btn, send, attach, gif_btn, toast, banner, side_stack, content_stack, composer,
            pending_box, pending: RefCell::default(), settings, notifier: crate::notify::Notifier::new(), new_chat, contacts: Rc::default() };
        v
    }

    pub fn set_on_session_expired(&self, f: impl Fn() + 'static) { *self.on_session_expired.borrow_mut() = Some(Box::new(f)); }

    pub fn start(self: &Rc<Self>) {
        {
            let me = Rc::downgrade(self);
            self.thread_scroll.vadjustment().connect_value_changed(move |_| { if let Some(me) = me.upgrade() { me.on_thread_scrolled(); } });
            let me = Rc::downgrade(self);
            // Row heights only become known after layout, so the range (`upper`) changes some time
            // after rows are appended. Apply the pending scroll target on every such change — on
            // the next frame: `changed` fires at the end of the viewport's allocation, after the
            // rows were placed, so moving the value from inside it leaves the rows drawn at the
            // old offset (a new message half hidden under the composer, with nowhere to scroll).
            self.thread_scroll.vadjustment().connect_changed(move |_| {
                let Some(me) = me.upgrade() else { return };
                if me.scroll_queued.replace(true) { return; }
                let weak = Rc::downgrade(&me);
                me.thread_scroll.add_tick_callback(move |_, _| {
                    if let Some(me) = weak.upgrade() { me.scroll_queued.set(false); me.apply_scroll_target(); }
                    glib::ControlFlow::Break
                });
            });
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
        // right-click menu actions (the popover itself is attached per row in `conv_row`)
        let group = gtk4::gio::SimpleActionGroup::new();
        let delete = gtk4::gio::SimpleAction::new("delete", Some(glib::VariantTy::STRING));
        let me = self.clone();
        delete.connect_activate(move |_, p| { if let Some(id) = p.and_then(|v| v.get::<String>()) { me.confirm_delete(id); } });
        group.add_action(&delete);
        self.list.insert_action_group("conv", Some(&group));
        // selection → open thread
        let me = self.clone();
        self.list.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            let id = unsafe { row.data::<String>("conv-id").map(|p| p.as_ref().clone()) }.unwrap_or_default();
            // `rebuild_list` re-selects the open conversation on every update; re-opening it
            // would re-render the thread and snap it to the bottom.
            if me.st.borrow().current.as_deref() == Some(&id) { me.mark_read(&id); } else { me.open(&id); }
        });
        // composer
        let me = self.clone();
        // Capture phase: the text view's own handler would otherwise insert the newline first.
        let keys = gtk4::EventControllerKey::builder().propagation_phase(gtk4::PropagationPhase::Capture).build();
        keys.connect_key_pressed(move |_, key, _, state| {
            let enter = matches!(key, gtk4::gdk::Key::Return | gtk4::gdk::Key::KP_Enter | gtk4::gdk::Key::ISO_Enter);
            if enter && !state.contains(gtk4::gdk::ModifierType::SHIFT_MASK) { me.send_current(); glib::Propagation::Stop } else { glib::Propagation::Proceed }
        });
        self.entry.add_controller(keys);
        // Paste with files or an image on the clipboard stages them as attachments, like Messages
        // for Web. Text wins when the clipboard also offers it (a spreadsheet cell copies as an
        // image too), except for a file list, which file managers pair with the paths as text.
        let me = self.clone();
        self.entry.connect_paste_clipboard(move |tv| {
            let cb = tv.clipboard();
            let f = cb.formats().union_deserialize_types();
            let files = f.contains_type(gtk4::gdk::FileList::static_type());
            let image = !f.contains_type(glib::Type::STRING) && f.contains_type(gtk4::gdk::Texture::static_type());
            if !files && !image { return; }
            tv.stop_signal_emission_by_name("paste-clipboard");
            me.paste_attachments(cb, files);
        });
        // The context menu has its own "Check Spelling" toggle, so the adapter is the one place
        // the choice lives; Preferences flips it too, and either way it is saved from here.
        let me = self.clone();
        self.spelling.connect_enabled_notify(move |a| {
            let mut settings = me.settings.borrow_mut();
            if settings.spell_check != a.is_enabled() { settings.spell_check = a.is_enabled(); settings.save(); }
        });
        let me = self.clone();
        self.send.connect_clicked(move |_| me.send_current());
        let me = self.clone();
        self.attach.connect_clicked(move |_| me.pick_and_send());
        self.build_gif_picker();
        // emoji picker: GTK's own chooser, anchored to the emoji button, inserting at the cursor
        let chooser = gtk4::EmojiChooser::new();
        chooser.set_parent(&self.emoji_btn);
        let entry = self.entry.clone();
        chooser.connect_emoji_picked(move |_, e| {
            let b = entry.buffer();
            b.delete_selection(true, true);
            b.insert_at_cursor(e);
        });
        let entry = self.entry.clone();
        chooser.connect_hide(move |_| { let entry = entry.clone(); glib::idle_add_local_once(move || { entry.grab_focus(); }); });
        self.emoji_btn.connect_clicked(move |_| {
            chooser.popup();
        });
        // typing indicator: notify the phone (throttled) while the user types
        let me = self.clone();
        let last = Rc::new(RefCell::new(std::time::Instant::now() - std::time::Duration::from_secs(10)));
        self.entry.buffer().connect_changed(move |b| {
            if b.char_count() == 0 || last.borrow().elapsed() < std::time::Duration::from_secs(4) { return; }
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
            Event::SessionExpired => {
                self.banner.set_title("Google expired this pairing — pick the emoji again to reconnect."); self.banner.set_revealed(true);
                // Take it: the phone answers every poll this way until we disconnect, and
                // re-entering the teardown would restart a re-pair that is already running.
                let cb = self.on_session_expired.borrow_mut().take();
                if let Some(cb) = cb { cb(); }
            }
            Event::Unpaired => {
                self.banner.set_title("This device was unpaired from the phone."); self.banner.set_revealed(true);
                let cb = self.on_session_expired.borrow_mut().take();
                if let Some(cb) = cb { cb(); }
            }
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
        if c.deleted { self.remove_conv(&c.id); return; }
        let mut st = self.st.borrow_mut();
        match st.convs.iter_mut().find(|x| x.id == c.id) {
            Some(x) => { let n = if c.unread { x.unread_count } else { 0 }; *x = c; x.unread_count = n; }
            None => st.convs.push(c),
        }
        st.convs.sort_by(|a, b| b.ts.cmp(&a.ts));
    }

    /// Drop a conversation from local state; closes the thread if it was the one open.
    fn remove_conv(&self, id: &str) {
        let mut st = self.st.borrow_mut();
        st.convs.retain(|c| c.id != id);
        st.messages.remove(id);
        st.cursors.remove(id);
        if st.current.as_deref() == Some(id) {
            st.current = None;
            drop(st);
            while let Some(r) = self.thread.row_at_index(0) { self.thread.remove(&r); }
            self.thread_title.set_title("");
            self.thread_title.set_subtitle("");
            self.composer.set_visible(false);
            self.content_stack.set_visible_child_name("empty");
            self.widget.set_show_content(false);
        }
    }

    /// Right-click on a conversation row: context menu (delete).
    fn confirm_delete(self: &Rc<Self>, id: String) {
        let name = self.st.borrow().convs.iter().find(|c| c.id == id).map(|c| c.name.clone()).unwrap_or_default();
        let dlg = adw::AlertDialog::new(Some("Delete conversation?"), Some(&format!("“{name}” and all its messages will be deleted from your phone.")));
        dlg.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dlg.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dlg.set_default_response(Some("cancel"));
        dlg.set_close_response("cancel");
        let me = self.clone();
        dlg.choose(Some(&self.win), None::<&gtk4::gio::Cancellable>, move |resp| {
            if resp != "delete" { return }
            let c = me.client.clone();
            let id2 = id.clone();
            let (tx, rx) = async_channel::bounded(1);
            crate::rt::spawn(async move { let _ = tx.send(c.delete_conversation(&id2).await).await; });
            let me = me.clone();
            glib::spawn_future_local(async move {
                match rx.recv().await {
                    Ok(Ok(())) => { me.remove_conv(&id); me.rebuild_list(); }
                    Ok(Err(e)) => me.toast.add_toast(adw::Toast::new(&format!("Could not delete conversation: {e:#}"))),
                    Err(_) => {}
                }
            });
        });
    }

    fn rebuild_list(&self) {
        let selected = self.st.borrow().current.clone();
        while let Some(r) = self.list.row_at_index(0) { self.list.remove(&r); }
        let convs = self.st.borrow().convs.clone();
        let mut rows = HashMap::new();
        for c in &convs {
            let row = conv_row(c, &self.avatars.borrow());
            self.list.append(&row);
            rows.insert(c.id.clone(), row);
        }
        let sel = selected.and_then(|id| rows.get(&id).cloned());
        self.st.borrow_mut().rows = rows;
        // Last: selecting runs the selection handler, which may rebuild the list itself.
        if let Some(row) = sel { self.list.select_row(Some(&row)); }
        self.side_stack.set_visible_child_name(if convs.is_empty() { "empty" } else { "list" });
    }

    /// Ask the phone for contact photos of every conversation participant we haven't resolved
    /// yet, and refresh the list when they land.
    fn fetch_avatars(self: &Rc<Self>) {
        let ids: Vec<String> = self.st.borrow().convs.iter().flat_map(|c| c.members.iter().map(|m| m.id.clone())).collect();
        let me = Rc::downgrade(self);
        self.request_avatars(ids, false, move || { if let Some(me) = me.upgrade() { me.refresh_avatars_in_place(); } });
    }

    /// Resolve photos for `ids` — from the on-disk cache where possible, otherwise from the phone
    /// in batched RPCs — and call `on_done` once anything new is available. Ids already cached or
    /// in flight are skipped, so calling this repeatedly is cheap. With `contacts` set, `ids` are
    /// `contact_key`s and the address-book thumbnail RPC is used instead of the participant one.
    fn request_avatars(self: &Rc<Self>, ids: Vec<String>, contacts: bool, on_done: impl Fn() + 'static) {
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
                let r = if contacts {
                    let raw: Vec<String> = chunk.iter().map(|k| k.trim_start_matches("contact:").to_string()).collect();
                    c.contact_thumbnails(&raw).await.map(|r| GetThumbnailResponse { thumbnail: r.thumbnail.into_iter().map(|mut t| { t.identifier = contact_key(&t.identifier); t }).collect() })
                } else { c.participant_thumbnails(chunk).await };
                match r {
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
            let Some(slot) = (unsafe { row.data::<gtk4::Overlay>("avatar-slot").map(|o| o.as_ref().clone()) }) else { continue };
            slot.set_child(Some(&conv_avatar(c, LIST_AVATAR, &avatars)));
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
        self.mark_read(id);
    }

    /// Tell the phone the conversation's latest message was seen, and clear its unread badge.
    fn mark_read(&self, id: &str) {
        let Some(conv) = self.st.borrow().convs.iter().find(|c| c.id == id).cloned() else { return };
        if !conv.unread || conv.latest_message_id.is_empty() { return; }
        let c = self.client.clone(); let (id2, mid) = (id.to_string(), conv.latest_message_id.clone());
        crate::rt::spawn(async move { let _ = c.mark_read(&id2, &mid).await; });
        if let Some(x) = self.st.borrow_mut().convs.iter_mut().find(|c| c.id == id) { x.unread = false; x.unread_count = 0; }
        self.rebuild_list();
    }

    fn push_message(self: &Rc<Self>, m: Msg, is_old: bool) {
        let conv_id = m.conversation_id.clone();
        let viewing = self.st.borrow().current.as_deref() == Some(&conv_id);
        // Decide before touching the list: follow the conversation if the user is at its end or
        // just sent something; otherwise hold their place while they read older messages.
        // Updates to a message we already have (status, reactions) only follow if already there.
        let is_new = !self.knows(&m);
        let adj = self.thread_scroll.vadjustment();
        let target = if (m.from_me && is_new) || self.at_bottom() || self.scroll_target.get() == ScrollTarget::Bottom { ScrollTarget::Bottom } else { ScrollTarget::FromBottom(adj.upper() - adj.value()) };
        {
            let mut st = self.st.borrow_mut();
            let list = st.messages.entry(conv_id.clone()).or_default();
            if let Some(x) = list.iter_mut().find(|x| x.id == m.id || (!m.tmp_id.is_empty() && x.tmp_id == m.tmp_id)) { *x = m.clone(); }
            else { list.push(m.clone()); list.sort_by_key(|m| m.ts); }
        }
        if is_new && !is_old && !m.from_me && !viewing {
            if let Some(c) = self.st.borrow_mut().convs.iter_mut().find(|c| c.id == conv_id) { c.unread = true; c.unread_count += 1; }
            self.rebuild_list();
        }
        if viewing { self.render_thread(target); }
    }

    /// Whether `m` (by id, or by tmp id for our own echo) is already in its conversation.
    fn knows(&self, m: &Msg) -> bool {
        self.st.borrow().messages.get(&m.conversation_id)
            .is_some_and(|l| l.iter().any(|x| x.id == m.id || (!m.tmp_id.is_empty() && x.tmp_id == m.tmp_id)))
    }

    fn render_thread(self: &Rc<Self>, target: ScrollTarget) {
        self.scroll_target.set(target);
        // Destroying the focused widget makes GTK move focus and scroll to wherever it lands.
        let focus = self.thread.root().and_then(|r| r.focus());
        if focus.is_some_and(|f| f.is_ancestor(&self.thread)) { self.entry.grab_focus(); }
        while let Some(r) = self.thread.row_at_index(0) { self.thread.remove(&r); }
        let st = self.st.borrow();
        let Some(cur) = &st.current else { return };
        let Some(msgs) = st.messages.get(cur) else { return };
        self.content_stack.set_visible_child_name("thread");
        let conv = st.convs.iter().find(|c| &c.id == cur);
        let group = conv.map(|c| c.is_group).unwrap_or(false);
        let self_ids = conv.map(|c| c.self_ids.clone()).unwrap_or_default();
        if st.cursors.get(cur).map(|c| c.is_some()).unwrap_or(false) {
            let spinner = adw::Spinner::builder().width_request(24).height_request(24).margin_top(8).margin_bottom(8).halign(gtk4::Align::Center).build();
            self.thread.append(&gtk4::ListBoxRow::builder().child(&spinner).activatable(false).selectable(false).build());
        }
        for m in msgs { self.thread.append(&self.bubble(m, group, &self_ids)); }
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

    fn entry_text(&self) -> String {
        let b = self.entry.buffer();
        b.text(&b.start_iter(), &b.end_iter(), false).to_string()
    }

    fn send_current(self: &Rc<Self>) {
        let text = self.entry_text();
        let has_pending = !self.pending.borrow().is_empty();
        if text.trim().is_empty() && !has_pending { return; }
        let conv = { let st = self.st.borrow(); st.current.as_ref().and_then(|id| st.convs.iter().find(|c| &c.id == id).cloned()) };
        let Some(conv) = conv else { return };
        self.entry.buffer().set_text("");
        if has_pending { self.send_pending(conv, text); return; }
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
    /// GIF search popover on the composer's GIF button: a debounced DuckDuckGo search filling a
    /// grid of thumbnails; clicking one downloads the GIF and sends it like any attachment.
    fn build_gif_picker(self: &Rc<Self>) {
        let pop = gtk4::Popover::builder().width_request(372).build();
        pop.set_parent(&self.gif_btn);
        let search = gtk4::SearchEntry::builder().placeholder_text("Search GIFs").build();
        let grid = gtk4::FlowBox::builder().selection_mode(gtk4::SelectionMode::None).min_children_per_line(3).max_children_per_line(3)
            .homogeneous(true).row_spacing(4).column_spacing(4).valign(gtk4::Align::Start).build();
        let scroll = gtk4::ScrolledWindow::builder().child(&grid).hscrollbar_policy(gtk4::PolicyType::Never).height_request(400).build();
        let status = gtk4::Label::builder().label("Type to search").css_classes(["dim-label"]).margin_top(12).margin_bottom(12).build();
        let col = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(6).build();
        col.append(&search); col.append(&status); col.append(&scroll);
        pop.set_child(Some(&col));
        let s = search.clone();
        pop.connect_show(move |_| { s.grab_focus(); });
        let p = pop.clone();
        self.gif_btn.connect_clicked(move |_| p.popup());

        // Each keystroke bumps the generation; a query only runs (and its results only land) if it
        // is still the latest one 350ms later.
        let generation = Rc::new(Cell::new(0u64));
        let me = self.clone();
        let grid2 = grid.clone();
        search.connect_search_changed(move |e| {
            let grid = &grid2;
            let query = e.text().to_string();
            generation.set(generation.get() + 1);
            let my_gen = generation.get();
            if query.trim().is_empty() { status.set_label("Type to search"); status.set_visible(true); Self::clear_flow(grid); return; }
            let (generation, status, grid, me) = (generation.clone(), status.clone(), grid.clone(), me.clone());
            glib::timeout_add_local_once(std::time::Duration::from_millis(350), move || {
                if generation.get() != my_gen { return; }
                status.set_label("Searching…"); status.set_visible(true);
                let (tx, rx) = async_channel::bounded(1);
                crate::rt::spawn(async move { let _ = tx.send(crate::gif::search(&query, 0).await).await; });
                glib::spawn_future_local(async move {
                    let Ok(res) = rx.recv().await else { return };
                    if generation.get() != my_gen { return; }
                    Self::clear_flow(&grid);
                    match res {
                        Ok(gifs) if gifs.is_empty() => status.set_label("No GIFs found"),
                        Ok(gifs) => { status.set_visible(false); for g in gifs.into_iter().take(45) { grid.append(&me.gif_tile(g)); } }
                        Err(e) => status.set_label(&format!("{e:#}")),
                    }
                });
            });
        });
        let p = pop.clone();
        // Popover clicks route via the tile's stored URL; close the popover once a GIF is chosen.
        let me = self.clone();
        grid.connect_child_activated(move |_, child| {
            let Some(url) = (unsafe { child.child().and_then(|c| c.data::<String>("gif-url")).map(|u| u.as_ref().clone()) }) else { return };
            p.popdown();
            me.send_gif(url);
        });
    }

    fn clear_flow(grid: &gtk4::FlowBox) {
        while let Some(c) = grid.first_child() { grid.remove(&c); }
    }

    /// A grid tile: a fixed-size picture whose thumbnail loads in the background.
    fn gif_tile(self: &Rc<Self>, g: crate::gif::Gif) -> gtk4::Widget {
        let pic = gtk4::Picture::builder().content_fit(gtk4::ContentFit::Cover).can_shrink(true).overflow(gtk4::Overflow::Hidden).build();
        pic.set_size_request(112, 112);
        let btn = gtk4::Button::builder().child(&pic).css_classes(["flat", "bubo-gif-tile"]).tooltip_text(&g.url).build();
        unsafe { btn.set_data("gif-url", g.url.clone()); }
        let thumb = g.thumbnail.clone();
        let (tx, rx) = async_channel::bounded(1);
        crate::rt::spawn(async move { let _ = tx.send(crate::gif::thumbnail(&thumb).await).await; });
        let weak = pic.downgrade();
        glib::spawn_future_local(async move {
            let Ok(Ok(bytes)) = rx.recv().await else { return };
            let Some(pic) = weak.upgrade() else { return };
            if let Ok(tex) = gtk4::gdk::Texture::from_bytes(&glib::Bytes::from(&bytes)) { pic.set_paintable(Some(&tex)); }
        });
        // FlowBox activates the child on click; the button itself just needs to not swallow it.
        let btn2 = btn.clone();
        btn.connect_clicked(move |_| { if let Some(child) = btn2.parent().and_downcast::<gtk4::FlowBoxChild>() { child.activate(); } });
        btn.upcast()
    }

    /// Download a GIF by URL, upload it, and send it (with any composer text as caption).
    fn send_gif(self: &Rc<Self>, url: String) {
        let conv = { let st = self.st.borrow(); st.current.as_ref().and_then(|id| st.convs.iter().find(|c| &c.id == id).cloned()) };
        let Some(conv) = conv else { return };
        self.toast.add_toast(adw::Toast::new("Sending GIF…"));
        let (tx, rx) = async_channel::bounded(1);
        let (c, cid, pid, caption) = (self.client.clone(), conv.id.clone(), conv.default_outgoing_id.clone(), self.entry_text());
        self.entry.buffer().set_text("");
        crate::rt::spawn(async move {
            let r = async {
                let data = crate::gif::download(&url).await?;
                let media = c.upload_media(&data, "animation.gif", "image/gif").await?;
                c.send_media(&cid, &pid, media, &caption, None).await.map(|_| ())
            }.await;
            let _ = tx.send(r).await;
        });
        let me = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(Err(e)) = rx.recv().await { me.toast.add_toast(adw::Toast::new(&format!("GIF send failed: {e:#}"))); }
        });
    }

    /// Read files or an image off the clipboard into the composer tray.
    fn paste_attachments(self: &Rc<Self>, cb: gtk4::gdk::Clipboard, files: bool) {
        let me = self.clone();
        glib::spawn_future_local(async move {
            let got = if files { Self::clipboard_files(&cb).await } else { Self::clipboard_image(&cb).await };
            match got {
                Ok(items) if !items.is_empty() => for (data, name, mime) in items { me.stage_attachment(data, name, mime); },
                // e.g. a uri-list of web links: nothing local to attach, so paste it as text after all
                Ok(_) => if let Ok(Some(t)) = cb.read_text_future().await {
                    let b = me.entry.buffer();
                    b.delete_selection(true, true);
                    b.insert_at_cursor(&t);
                },
                Err(e) => me.toast.add_toast(adw::Toast::new(&format!("Could not paste: {e:#}"))),
            }
        });
    }

    /// Local files from a copied file list (non-local URIs and directories are skipped).
    async fn clipboard_files(cb: &gtk4::gdk::Clipboard) -> anyhow::Result<Vec<(Vec<u8>, String, String)>> {
        let v = cb.read_value_future(gtk4::gdk::FileList::static_type(), glib::Priority::DEFAULT).await?;
        let list = v.get::<gtk4::gdk::FileList>().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut out = Vec::new();
        for path in list.files().iter().filter_map(|f| f.path()).filter(|p| p.is_file()) {
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("file").to_string();
            let data = std::fs::read(&path).map_err(|e| anyhow::anyhow!("{name}: {e}"))?;
            let mime = gtk4::gio::content_type_guess(Some(&name), Some(data.as_slice())).0.to_string();
            out.push((data, name, mime));
        }
        Ok(out)
    }

    /// The clipboard image, as its original bytes when it's a type we can send as-is.
    async fn clipboard_image(cb: &gtk4::gdk::Clipboard) -> anyhow::Result<Vec<(Vec<u8>, String, String)>> {
        if let Ok((stream, mime)) = cb.read_future(PASTE_IMAGE_MIMES, glib::Priority::DEFAULT).await {
            let mut data = Vec::new();
            loop {
                let chunk = stream.read_bytes_future(64 * 1024, glib::Priority::DEFAULT).await?;
                if chunk.is_empty() { break; }
                data.extend_from_slice(&chunk);
            }
            let ext = mime.strip_prefix("image/").unwrap_or("png");
            return Ok(vec![(data, format!("pasted.{ext}"), mime.to_string())]);
        }
        let Some(tex) = cb.read_texture_future().await? else { anyhow::bail!("clipboard has no image") };
        Ok(vec![(tex.save_to_png_bytes().to_vec(), "pasted.png".into(), "image/png".into())])
    }

    /// Add a file to the composer tray: a thumbnail for images, an icon and name otherwise.
    fn stage_attachment(self: &Rc<Self>, data: Vec<u8>, name: String, mime: String) {
        let thumb = if mime.starts_with("image/") { Self::square_thumbnail(&data, 160) } else { None };
        let preview: gtk4::Widget = match thumb {
            // An Image draws at its pixel size whatever the paintable's own size; a Picture would
            // ask for the full image dimensions and blow the tray up.
            Some(tex) => gtk4::Image::builder().paintable(&tex).pixel_size(80).overflow(gtk4::Overflow::Hidden)
                .css_classes(["bubo-pending-tile"]).build().upcast(),
            None => {
                let b = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(4).valign(gtk4::Align::Center).css_classes(["card", "bubo-pending-tile"]).build();
                b.append(&gtk4::Image::builder().gicon(&gtk4::gio::content_type_get_symbolic_icon(&mime)).pixel_size(24).margin_top(8).build());
                b.append(&gtk4::Label::builder().label(&name).ellipsize(gtk4::pango::EllipsizeMode::Middle).max_width_chars(8).margin_start(4).margin_end(4).css_classes(["caption"]).build());
                b.upcast()
            }
        };
        preview.set_size_request(80, 80);
        preview.set_tooltip_text(Some(&name));
        let overlay = gtk4::Overlay::builder().child(&preview).build();
        let remove = gtk4::Button::builder().icon_name("window-close-symbolic").css_classes(["circular", "osd", "bubo-pending-remove"])
            .halign(gtk4::Align::End).valign(gtk4::Align::Start).tooltip_text("Remove").build();
        overlay.add_overlay(&remove);
        let tile: gtk4::Widget = overlay.upcast();
        let me = self.clone();
        let t = tile.clone();
        remove.connect_clicked(move |_| {
            me.pending.borrow_mut().retain(|p| p.tile != t);
            me.pending_box.remove(&t);
            me.pending_box.set_visible(!me.pending.borrow().is_empty());
            me.entry.grab_focus();
        });
        self.pending_box.append(&tile);
        self.pending_box.set_visible(true);
        self.pending.borrow_mut().push(Pending { data, name, mime, tile });
    }

    /// A centre-cropped `side`×`side` texture of an image, for tray tiles (2× the tile size so it
    /// stays sharp on HiDPI). GIFs give their first frame.
    fn square_thumbnail(data: &[u8], side: i32) -> Option<gtk4::gdk::Texture> {
        let stream = gtk4::gio::MemoryInputStream::from_bytes(&glib::Bytes::from(data));
        let pb = gtk4::gdk_pixbuf::Pixbuf::from_stream(&stream, None::<&gtk4::gio::Cancellable>).ok()?;
        let (w, h) = (pb.width(), pb.height());
        let s = w.min(h);
        let sq = pb.new_subpixbuf((w - s) / 2, (h - s) / 2, s, s);
        let scaled = sq.scale_simple(side, side, gtk4::gdk_pixbuf::InterpType::Bilinear)?;
        Some(gtk4::gdk::Texture::for_pixbuf(&scaled))
    }

    /// Upload and send everything in the tray, in order; the composer text rides as the caption
    /// on the last one so it lands beneath the pictures.
    fn send_pending(self: &Rc<Self>, conv: Conv, text: String) {
        let items: Vec<(Vec<u8>, String, String)> = self.pending.borrow_mut().drain(..).map(|p| {
            self.pending_box.remove(&p.tile);
            (p.data, p.name, p.mime)
        }).collect();
        self.pending_box.set_visible(false);
        let n = items.len();
        self.toast.add_toast(adw::Toast::new(&if n == 1 { format!("Sending {}…", items[0].1) } else { format!("Sending {n} attachments…") }));
        let caption = if text.trim().is_empty() { String::new() } else { text };
        let (tx, rx) = async_channel::bounded(1);
        let (c, cid, pid) = (self.client.clone(), conv.id.clone(), conv.default_outgoing_id.clone());
        crate::rt::spawn(async move {
            let r = async {
                for (i, (data, name, mime)) in items.into_iter().enumerate() {
                    let media = c.upload_media(&data, &name, &mime).await?;
                    c.send_media(&cid, &pid, media, if i + 1 == n { &caption } else { "" }, None).await?;
                }
                anyhow::Ok(())
            }.await;
            let _ = tx.send(r).await;
        });
        let me = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(Err(e)) = rx.recv().await { me.toast.add_toast(adw::Toast::new(&format!("Send failed: {e:#}"))); }
        });
    }

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
            let (c, cid, pid, caption) = (me.client.clone(), conv.id.clone(), conv.default_outgoing_id.clone(), me.entry_text());
            crate::rt::spawn(async move {
                let r = match c.upload_media(&data, &name, &mime).await {
                    Ok(media) => c.send_media(&cid, &pid, media, &caption, None).await.map(|_| ()),
                    Err(e) => Err(e),
                };
                let _ = tx.send(r).await;
            });
            me.entry.buffer().set_text("");
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
        // Status and reaction changes re-send the whole message; only its first sighting is news.
        // One we never loaded that already carries reactions is an old message being reacted to.
        if self.knows(m) { return; }
        let age_us = glib::real_time() - m.ts;
        if !m.reactions.is_empty() && age_us > 60_000_000 { return; }
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
                let key = contact_key(&c.contact_id);
                if let Some(Some(tex)) = avatars.get(&key).or_else(|| avatars.get(&c.participant_id)) { av.set_custom_image(Some(tex)); }
                row.add_prefix(&av);
                unsafe { row.set_data("number", c.number.clone()); if !c.contact_id.is_empty() { row.set_data("pid", key); } }
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
            me.request_avatars(ids, true, move || p());
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
                            Some(ContactEntry { name, number, formatted, participant_id: c.participant_id, contact_id: c.contact_id })
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
        let appearance = adw::PreferencesGroup::builder().title("Appearance").build();
        let follow = adw::SwitchRow::builder().title("Follow Omarchy theme")
            .subtitle(crate::omarchy::theme_name().map(|name| format!("Match the desktop colors · {name}"))
                .unwrap_or_else(|| "Match the desktop colors".into()))
            .active(self.settings.borrow().follow_omarchy_theme).build();
        let me = self.clone();
        follow.connect_active_notify(move |row| {
            let mut settings = me.settings.borrow_mut();
            settings.follow_omarchy_theme = row.is_active();
            settings.save();
            crate::omarchy::set_follow(row.is_active());
        });
        appearance.add(&follow);
        appearance.set_visible(crate::omarchy::detected());
        page.add(&appearance);
        let composing = adw::PreferencesGroup::builder().title("Composing").build();
        let spell = adw::SwitchRow::builder().title("Check spelling").subtitle("Underline misspelled words while typing")
            .active(self.spelling.is_enabled()).build();
        let spelling = self.spelling.clone();
        spell.connect_active_notify(move |row| spelling.set_enabled(row.is_active()));
        composing.add(&spell);
        page.add(&composing);
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
    const SIZE: i32 = LIST_AVATAR;
    let overlay = gtk4::Overlay::builder().child(&conv_avatar(c, SIZE, avatars)).valign(gtk4::Align::Center).build();
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
    unsafe { row.set_data("conv-id", c.id.clone()); row.set_data("avatar-slot", overlay.clone()); }

    // Right-click (or long-press) → context menu. Actions live in the "conv" group on the list.
    let menu = gtk4::gio::Menu::new();
    let del = gtk4::gio::MenuItem::new(Some("Delete conversation"), None);
    del.set_action_and_target_value(Some("conv.delete"), Some(&c.id.to_variant()));
    menu.append_item(&del);
    let popover = gtk4::PopoverMenu::from_model(Some(&menu));
    popover.set_parent(&row);
    popover.set_has_arrow(false);
    popover.set_halign(gtk4::Align::Start);
    popover.connect_closed(|p| p.set_visible(false));
    let pop = popover.clone();
    let show = move |x: f64, y: f64| { pop.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1))); pop.popup(); };
    let click = gtk4::GestureClick::builder().button(gtk4::gdk::BUTTON_SECONDARY).build();
    let s = show.clone();
    click.connect_pressed(move |g, _, x, y| { g.set_state(gtk4::EventSequenceState::Claimed); s(x, y); });
    row.add_controller(click);
    let press = gtk4::GestureLongPress::builder().touch_only(true).build();
    press.connect_pressed(move |g, x, y| { g.set_state(gtk4::EventSequenceState::Claimed); show(x, y); });
    row.add_controller(press);
    // Unparent the popover when the row goes away, or GTK warns about a dangling child.
    row.connect_destroy(move |_| popover.unparent());
    row
}

/// Side of a conversation's picture in the sidebar.
const LIST_AVATAR: i32 = 40;

/// One person's picture: their contact photo if the phone has one, otherwise their initials if
/// they're in the address book, otherwise a blank silhouette (initials of a phone number are noise).
fn person_avatar(size: i32, name: &str, id: &str, is_contact: bool, avatars: &HashMap<String, Option<gtk4::gdk::Texture>>) -> adw::Avatar {
    // The text also seeds the background colour, so give bare numbers their id to vary it.
    let av = adw::Avatar::new(size, Some(if name.is_empty() { id } else { name }), is_contact);
    if let Some(Some(tex)) = avatars.get(id) { av.set_custom_image(Some(tex)); }
    av
}

/// A conversation's picture: the other person's for a 1:1 chat, and for a group a mosaic of up
/// to four members' pictures packed into the same circle, Google-Messages style.
fn conv_avatar(c: &Conv, size: i32, avatars: &HashMap<String, Option<gtk4::gdk::Texture>>) -> gtk4::Widget {
    if !c.is_group || c.members.len() < 2 {
        let Some(m) = c.members.first() else {
            let av = adw::Avatar::new(size, Some(&c.name), false);
            if c.is_group { av.set_icon_name(Some("system-users-symbolic")); }
            return av.upcast();
        };
        // The conversation name is how the phone labels the person, so take initials from it.
        let name = if c.is_group { &m.name } else { &c.name };
        return person_avatar(size, name, &m.id, m.is_contact, avatars).upcast();
    }
    let (s, spots) = match c.members.len() {
        // Two: diagonal, top-left to bottom-right, just clear of each other.
        2 => { let s = size * 11 / 20; (s, vec![(0, 0), (size - s, size - s)]) }
        // Three: one on top, two below.
        3 => { let s = size / 2; (s, vec![((size - s) / 2, 0), (0, size - s), (size - s, size - s)]) }
        // Four or more: a 2x2 grid of the first four.
        _ => { let s = size / 2 - 1; (s, vec![(0, 0), (size - s, 0), (0, size - s), (size - s, size - s)]) }
    };
    let grid = gtk4::Fixed::builder().width_request(size).height_request(size).build();
    for (m, (x, y)) in c.members.iter().zip(spots) {
        grid.put(&person_avatar(s, &m.name, &m.id, m.is_contact, avatars), x as f64, y as f64);
    }
    grid.upcast()
}

impl ChatsView {
    fn bubble(self: &Rc<Self>, m: &Msg, group: bool, self_ids: &[String]) -> gtk4::ListBoxRow {
    let halign = if m.from_me { gtk4::Align::End } else { gtk4::Align::Start };
    let col = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(4).halign(halign).build();
    let bubble = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(6).css_classes(["bubo-bubble", if m.from_me { "bubo-me" } else { "bubo-them" }]).halign(halign).build();
    // Group chats: attribute each message Google-Messages style — a small contact photo and the
    // sender's full name in their avatar colour, sitting above the bubble rather than inside it.
    if group && !m.from_me && !m.sender_full.is_empty() {
        let av = person_avatar(24, &m.sender_full, &m.sender_id, m.sender_is_contact, &self.avatars.borrow());
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
    let mine = my_reaction(&m.reactions, self_ids);
    if !m.reactions.is_empty() {
        let chips = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(2).halign(halign).css_classes(["bubo-reactions"]).build();
        for r in &m.reactions {
            let label = if r.participant_ids.len() > 1 { format!("{} {}", r.emoji, r.participant_ids.len()) } else { r.emoji.clone() };
            let is_mine = mine.is_some_and(|e| e == r.emoji);
            let chip = gtk4::Button::builder().label(&label).css_classes(["bubo-reaction"]).focus_on_click(false)
                .tooltip_text(if is_mine { "Remove your reaction".to_string() } else { format!("React with {}", r.emoji) }).build();
            if is_mine { chip.add_css_class("bubo-reaction-mine"); }
            let (me, cid, mid, e) = (self.clone(), m.conversation_id.clone(), m.id.clone(), r.emoji.clone());
            chip.connect_clicked(move |_| me.react(&cid, &mid, &e));
            chips.append(&chip);
        }
        col.append(&chips);
    }
    // Right-click (or long-press on touch) opens the reaction menu. It runs in the capture phase
    // so it wins over the selectable label's own menu; that menu's "Copy" lives in ours instead.
    if !m.id.is_empty() {
        let click = gtk4::GestureClick::builder().button(gtk4::gdk::BUTTON_SECONDARY).propagation_phase(gtk4::PropagationPhase::Capture).build();
        let (me, msg, w) = (self.clone(), m.clone(), col.downgrade());
        click.connect_pressed(move |g, _, x, y| {
            g.set_state(gtk4::EventSequenceState::Claimed);
            if let Some(w) = w.upgrade() { me.reaction_menu(&w, x, y, &msg); }
        });
        col.add_controller(click);
        let press = gtk4::GestureLongPress::builder().touch_only(true).propagation_phase(gtk4::PropagationPhase::Capture).build();
        let (me, msg, w) = (self.clone(), m.clone(), col.downgrade());
        press.connect_pressed(move |g, x, y| {
            g.set_state(gtk4::EventSequenceState::Claimed);
            if let Some(w) = w.upgrade() { me.reaction_menu(&w, x, y, &msg); }
        });
        col.add_controller(press);
    }
    let mut meta = fmt_time(m.ts);
    if m.from_me { meta.push_str(match m.status { 1 | 2 | 3 | 4 | 5 | 6 => " · sent", 11 => " · delivered", 12 => " · read", s if s >= 100 => " · failed", _ => "" }); }
    let meta = gtk4::Label::builder().label(&meta).css_classes(["bubo-meta"]).halign(halign).build();
    col.append(&meta);
    gtk4::ListBoxRow::builder().child(&col).activatable(false).selectable(false).build()
    }

    /// The menu a right-click on a message opens: the quick reactions (ours highlighted), a "+"
    /// for any other emoji, and "Copy text" when the message has some.
    fn reaction_menu(self: &Rc<Self>, anchor: &gtk4::Box, x: f64, y: f64, m: &Msg) {
        let self_ids = { let st = self.st.borrow(); st.convs.iter().find(|c| c.id == m.conversation_id).map(|c| c.self_ids.clone()).unwrap_or_default() };
        let mine = my_reaction(&m.reactions, &self_ids).map(str::to_owned);
        let at = gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1);
        let pop = gtk4::Popover::builder().has_arrow(false).pointing_to(&at).build();
        pop.set_parent(anchor);
        // Popovers made per click are dropped again once closed, so rows don't collect them.
        pop.connect_closed(|p| { let p = p.clone(); glib::idle_add_local_once(move || p.unparent()); });
        let body = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
        for &e in QUICK_REACTIONS {
            let b = gtk4::Button::builder().label(e).css_classes(["flat", "circular", "bubo-react-pick"]).build();
            if mine.as_deref().is_some_and(|x| crate::gm::client::same_emoji(x, e)) { b.add_css_class("bubo-reaction-mine"); }
            let (me, cid, mid, p) = (self.clone(), m.conversation_id.clone(), m.id.clone(), pop.downgrade());
            b.connect_clicked(move |_| { if let Some(p) = p.upgrade() { p.popdown(); } me.react(&cid, &mid, e); });
            row.append(&b);
        }
        let more = gtk4::Button::builder().icon_name("list-add-symbolic").css_classes(["flat", "circular", "bubo-react-pick"]).tooltip_text("More reactions").build();
        let (me, cid, mid, p, a) = (self.clone(), m.conversation_id.clone(), m.id.clone(), pop.downgrade(), anchor.downgrade());
        more.connect_clicked(move |_| {
            if let Some(p) = p.upgrade() { p.popdown(); }
            let Some(a) = a.upgrade() else { return };
            let chooser = gtk4::EmojiChooser::builder().pointing_to(&at).build();
            chooser.set_parent(&a);
            let (me, cid, mid) = (me.clone(), cid.clone(), mid.clone());
            chooser.connect_emoji_picked(move |_, e| me.react(&cid, &mid, e));
            chooser.connect_closed(|c| { let c = c.clone(); glib::idle_add_local_once(move || c.unparent()); });
            chooser.popup();
        });
        row.append(&more);
        body.append(&row);
        if !m.text.trim().is_empty() {
            let copy = gtk4::Button::builder().label("Copy text").css_classes(["flat"]).build();
            let (text, p) = (m.text.clone(), pop.downgrade());
            copy.connect_clicked(move |b| { b.clipboard().set_text(&text); if let Some(p) = p.upgrade() { p.popdown(); } });
            body.append(&copy);
        }
        pop.set_child(Some(&body));
        pop.popup();
    }

    /// React to a message the way the Messages app does: one reaction per person, so picking our
    /// current emoji again takes it back and picking another switches to it. The thread updates
    /// at once; the phone's echo of the message then confirms it, or a failure puts it back.
    fn react(self: &Rc<Self>, conv_id: &str, msg_id: &str, emoji: &str) {
        use crate::gm::proto::client::send_reaction_request::Action;
        let (self_ids, me_id, before) = {
            let st = self.st.borrow();
            let Some(conv) = st.convs.iter().find(|c| c.id == conv_id) else { return };
            let Some(m) = st.messages.get(conv_id).and_then(|l| l.iter().find(|m| m.id == msg_id)) else { return };
            let me_id = conv.self_ids.first().cloned().unwrap_or_else(|| "me".into());
            (conv.self_ids.clone(), me_id, m.reactions.clone())
        };
        let action = match my_reaction(&before, &self_ids) {
            Some(e) if crate::gm::client::same_emoji(e, emoji) => Action::Remove,
            Some(_) => Action::Switch,
            None => Action::Add,
        };
        let mut after: Vec<Reaction> = before.iter().cloned()
            .map(|mut r| { r.participant_ids.retain(|p| !self_ids.contains(p)); r })
            .filter(|r| !r.participant_ids.is_empty()).collect();
        if action != Action::Remove {
            match after.iter_mut().find(|r| crate::gm::client::same_emoji(&r.emoji, emoji)) {
                Some(r) => r.participant_ids.push(me_id),
                None => after.push(Reaction { emoji: emoji.into(), participant_ids: vec![me_id] }),
            }
        }
        self.set_reactions(conv_id, msg_id, after);
        let (tx, rx) = async_channel::bounded(1);
        let (c, mid, e) = (self.client.clone(), msg_id.to_owned(), emoji.to_owned());
        crate::rt::spawn(async move { let _ = tx.send(c.send_reaction(&mid, &e, action).await).await; });
        let (me, cid, mid) = (self.clone(), conv_id.to_owned(), msg_id.to_owned());
        glib::spawn_future_local(async move {
            if let Ok(Err(e)) = rx.recv().await {
                me.toast.add_toast(adw::Toast::new(&format!("Reaction failed: {e:#}")));
                me.set_reactions(&cid, &mid, before);
            }
        });
    }

    fn set_reactions(self: &Rc<Self>, conv_id: &str, msg_id: &str, reactions: Vec<Reaction>) {
        {
            let mut st = self.st.borrow_mut();
            let Some(m) = st.messages.get_mut(conv_id).and_then(|l| l.iter_mut().find(|m| m.id == msg_id)) else { return };
            m.reactions = reactions;
        }
        if self.st.borrow().current.as_deref() != Some(conv_id) { return; }
        let adj = self.thread_scroll.vadjustment();
        self.render_thread(if self.at_bottom() { ScrollTarget::Bottom } else { ScrollTarget::FromBottom(adj.upper() - adj.value()) });
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
