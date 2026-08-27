//! Conversation list + message thread + composer.
use super::state::{Conv, Media, Msg, fmt_time};
use crate::gm::client::Client;
use crate::gm::events::Event;
use crate::gm::proto::client::list_conversations_request::Folder;
use adw::prelude::*;
use gtk4::glib;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

#[derive(Default)]
struct State {
    convs: Vec<Conv>,
    messages: HashMap<String, Vec<Msg>>,
    current: Option<String>,
    rows: HashMap<String, gtk4::ListBoxRow>,
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
    thread_title: adw::WindowTitle,
    entry: gtk4::Entry,
    send: gtk4::Button,
    attach: gtk4::Button,
    toast: adw::ToastOverlay,
    banner: adw::Banner,
}

impl ChatsView {
    pub fn new(win: &adw::ApplicationWindow, client: Arc<Client>, events: async_channel::Receiver<Event>) -> Self {
        // ── sidebar ──
        let list = gtk4::ListBox::builder().selection_mode(gtk4::SelectionMode::Single).css_classes(["navigation-sidebar"]).build();
        let side_scroll = gtk4::ScrolledWindow::builder().child(&list).hscrollbar_policy(gtk4::PolicyType::Never).vexpand(true).build();
        let side_header = adw::HeaderBar::builder().title_widget(&adw::WindowTitle::new("Bubo", "")).build();
        let menu = gtk4::gio::Menu::new();
        menu.append(Some("Unpair phone"), Some("app.unpair"));
        side_header.pack_end(&gtk4::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).build());
        let side = adw::ToolbarView::new();
        side.add_top_bar(&side_header);
        side.set_content(Some(&side_scroll));
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
        let banner = adw::Banner::builder().revealed(false).build();
        let content_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        content_box.append(&banner); content_box.append(&thread_scroll); content_box.append(&composer);
        let content = adw::ToolbarView::new();
        content.add_top_bar(&adw::HeaderBar::builder().title_widget(&thread_title).build());
        content.set_content(Some(&content_box));
        let toast = adw::ToastOverlay::new(); toast.set_child(Some(&content));
        let content_page = adw::NavigationPage::builder().title("Conversation").child(&toast).build();

        let widget = adw::NavigationSplitView::builder().sidebar(&sidebar).content(&content_page).min_sidebar_width(260.0).max_sidebar_width(360.0).build();

        let css = gtk4::CssProvider::new();
        css.load_from_string("
            .bubo-bubble { padding: 8px 12px; border-radius: 16px; }
            .bubo-me { background: @accent_bg_color; color: @accent_fg_color; }
            .bubo-them { background: alpha(currentColor, 0.08); }
            .bubo-thread row { background: transparent; border: none; box-shadow: none; padding: 0; margin: 2px 0; }
            .bubo-meta { font-size: 0.8em; opacity: 0.7; }
            .bubo-snippet { opacity: 0.7; }
        ");
        gtk4::style_context_add_provider_for_display(&gtk4::gdk::Display::default().unwrap(), &css, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);

        let v = Self { widget, win: win.clone(), client, events, st: Rc::default(), list, thread, thread_scroll, thread_title, entry, send, attach, toast, banner };
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
        // notification click → focus window and open that conversation
        let open = gtk4::gio::SimpleAction::new("open-conversation", Some(glib::VariantTy::STRING));
        let me = self.clone();
        open.connect_activate(move |_, param| {
            let Some(id) = param.and_then(|p| p.str()) else { return };
            me.win.present();
            if let Some(row) = me.st.borrow().rows.get(id) { me.list.select_row(Some(row)); } else { me.open(id); }
        });
        if let Some(app) = self.win.application() { app.add_action(&open); }
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
                Ok(Ok(r)) => { for c in &r.conversations { me.upsert_conv(Conv::from_proto(c)); } me.rebuild_list(); }
                Ok(Err(e)) => me.toast.add_toast(adw::Toast::new(&format!("Could not load chats: {e:#}"))),
                Err(_) => {}
            }
        });
    }

    fn handle(self: &Rc<Self>, ev: Event) {
        match ev {
            Event::Conversation(c) => { self.upsert_conv(Conv::from_proto(&c)); self.rebuild_list(); }
            Event::Message { msg, is_old } => { let m = Msg::from_proto(&msg); self.maybe_notify(&m, is_old); self.push_message(m); }
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
        match st.convs.iter_mut().find(|x| x.id == c.id) { Some(x) => *x = c, None => st.convs.push(c) }
        st.convs.sort_by(|a, b| b.ts.cmp(&a.ts));
    }

    fn rebuild_list(&self) {
        let selected = self.st.borrow().current.clone();
        while let Some(r) = self.list.row_at_index(0) { self.list.remove(&r); }
        let convs = self.st.borrow().convs.clone();
        let mut rows = HashMap::new();
        for c in &convs {
            let row = conv_row(c);
            self.list.append(&row);
            if Some(&c.id) == selected.as_ref() { self.list.select_row(Some(&row)); }
            rows.insert(c.id.clone(), row);
        }
        self.st.borrow_mut().rows = rows;
    }

    fn open(self: &Rc<Self>, id: &str) {
        let conv = self.st.borrow().convs.iter().find(|c| c.id == id).cloned();
        let Some(conv) = conv else { return };
        self.st.borrow_mut().current = Some(id.to_string());
        self.thread_title.set_title(&conv.name);
        self.thread_title.set_subtitle(if conv.is_rcs { "RCS" } else { "SMS/MMS" });
        self.widget.set_show_content(true);
        self.entry.grab_focus();
        self.render_thread();
        if !self.st.borrow().messages.contains_key(id) {
            let c = self.client.clone(); let id2 = id.to_string();
            let (tx, rx) = async_channel::bounded(1);
            crate::rt::spawn(async move { let _ = tx.send(c.list_messages(&id2, 50, None).await).await; });
            let me = self.clone(); let id2 = id.to_string();
            glib::spawn_future_local(async move {
                match rx.recv().await {
                    Ok(Ok(r)) => {
                        let mut msgs: Vec<Msg> = r.messages.iter().map(Msg::from_proto).collect();
                        msgs.sort_by_key(|m| m.ts);
                        me.st.borrow_mut().messages.insert(id2.clone(), msgs);
                        if me.st.borrow().current.as_deref() == Some(&id2) { me.render_thread(); }
                    }
                    Ok(Err(e)) => me.toast.add_toast(adw::Toast::new(&format!("Could not load messages: {e:#}"))),
                    Err(_) => {}
                }
            });
        }
        if conv.unread && !conv.latest_message_id.is_empty() {
            let c = self.client.clone(); let (id2, mid) = (id.to_string(), conv.latest_message_id.clone());
            crate::rt::spawn(async move { let _ = c.mark_read(&id2, &mid).await; });
            if let Some(x) = self.st.borrow_mut().convs.iter_mut().find(|c| c.id == id) { x.unread = false; }
            self.rebuild_list();
        }
    }

    fn push_message(self: &Rc<Self>, m: Msg) {
        let conv_id = m.conversation_id.clone();
        {
            let mut st = self.st.borrow_mut();
            let list = st.messages.entry(conv_id.clone()).or_default();
            if let Some(x) = list.iter_mut().find(|x| x.id == m.id || (!m.tmp_id.is_empty() && x.tmp_id == m.tmp_id)) { *x = m.clone(); }
            else { list.push(m.clone()); list.sort_by_key(|m| m.ts); }
        }
        if self.st.borrow().current.as_deref() == Some(&conv_id) { self.render_thread(); }
    }

    fn render_thread(self: &Rc<Self>) {
        while let Some(r) = self.thread.row_at_index(0) { self.thread.remove(&r); }
        let st = self.st.borrow();
        let Some(cur) = &st.current else { return };
        let Some(msgs) = st.messages.get(cur) else { return };
        let group = st.convs.iter().find(|c| &c.id == cur).map(|c| c.is_group).unwrap_or(false);
        for m in msgs { self.thread.append(&self.bubble(m, group)); }
        drop(st);
        let sw = self.thread_scroll.clone();
        glib::idle_add_local_once(move || { let adj = sw.vadjustment(); adj.set_value(adj.upper() - adj.page_size()); });
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
    fn maybe_notify(&self, m: &Msg, is_old: bool) {
        if is_old || m.from_me { return; }
        let st = self.st.borrow();
        let focused_here = self.win.is_active() && st.current.as_deref() == Some(&m.conversation_id);
        if focused_here { return; }
        let conv = st.convs.iter().find(|c| c.id == m.conversation_id);
        let title = conv.filter(|c| !c.name.is_empty()).map(|c| c.name.clone())
            .or_else(|| (!m.sender.is_empty()).then(|| m.sender.clone()))
            .unwrap_or_else(|| "New message".into());
        // In a group, prefix the sender so you know who spoke.
        let body = match (conv.map(|c| c.is_group).unwrap_or(false), m.text.trim().is_empty()) {
            (_, true) if !m.media.is_empty() => "📎 Attachment".to_string(),
            (true, _) if !m.sender.is_empty() => format!("{}: {}", m.sender, m.text),
            _ => m.text.clone(),
        };
        drop(st);
        let Some(app) = self.win.application() else { return };
        let notif = gtk4::gio::Notification::new(&title);
        notif.set_body(Some(&body));
        notif.set_default_action_and_target_value("app.open-conversation", Some(&m.conversation_id.to_variant()));
        // One notification id per conversation, so a new message replaces the old bubble.
        app.send_notification(Some(&format!("bubo-{}", m.conversation_id)), &notif);
    }
}

fn conv_row(c: &Conv) -> gtk4::ListBoxRow {
    let avatar = adw::Avatar::new(40, Some(&c.name), true);
    if c.is_group { avatar.set_icon_name(Some("system-users-symbolic")); }
    let name = gtk4::Label::builder().label(&c.name).xalign(0.0).ellipsize(gtk4::pango::EllipsizeMode::End).hexpand(true).build();
    if c.unread { name.add_css_class("heading"); }
    let time = gtk4::Label::builder().label(fmt_time(c.ts)).css_classes(["bubo-meta"]).build();
    let snippet = gtk4::Label::builder().label(&c.snippet).xalign(0.0).ellipsize(gtk4::pango::EllipsizeMode::End).css_classes(["bubo-snippet", "caption"]).build();
    let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 6); top.append(&name); top.append(&time);
    let col = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(2).hexpand(true).valign(gtk4::Align::Center).build();
    col.append(&top); col.append(&snippet);
    let row_box = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(10).margin_top(6).margin_bottom(6).margin_start(4).margin_end(4).build();
    row_box.append(&avatar); row_box.append(&col);
    if c.unread { row_box.append(&gtk4::Image::builder().icon_name("media-record-symbolic").pixel_size(10).css_classes(["accent"]).build()); }
    let row = gtk4::ListBoxRow::builder().child(&row_box).build();
    unsafe { row.set_data("conv-id", c.id.clone()); }
    row
}

impl ChatsView {
    fn bubble(self: &Rc<Self>, m: &Msg, group: bool) -> gtk4::ListBoxRow {
    let bubble = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(6).css_classes(["bubo-bubble", if m.from_me { "bubo-me" } else { "bubo-them" }]).build();
    if group && !m.from_me && !m.sender.is_empty() {
        bubble.append(&gtk4::Label::builder().label(&m.sender).xalign(0.0).css_classes(["caption", "heading"]).build());
    }
    for md in &m.media { bubble.append(&self.attachment_widget(md)); }
    if !m.text.trim().is_empty() {
        bubble.append(&gtk4::Label::builder().label(&m.text).wrap(true).wrap_mode(gtk4::pango::WrapMode::WordChar).xalign(0.0).selectable(true).max_width_chars(60).build());
    }
    let mut meta = fmt_time(m.ts);
    if m.from_me { meta.push_str(match m.status { 1 | 2 | 3 | 4 | 5 | 6 => " · sent", 11 => " · delivered", 12 => " · read", s if s >= 100 => " · failed", _ => "" }); }
    let meta = gtk4::Label::builder().label(&meta).css_classes(["bubo-meta"]).halign(if m.from_me { gtk4::Align::End } else { gtk4::Align::Start }).build();
    let col = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(1).halign(if m.from_me { gtk4::Align::End } else { gtk4::Align::Start }).build();
    col.append(&bubble); col.append(&meta);
    gtk4::ListBoxRow::builder().child(&col).activatable(false).selectable(false).build()
    }

    /// A clickable attachment: images load inline on click; other files save to ~/Downloads.
    fn attachment_widget(self: &Rc<Self>, md: &Media) -> gtk4::Widget {
        let icon = if md.is_image() { "🖼" } else { "📎" };
        let btn = gtk4::Button::builder().label(&format!("{icon} {}", md.label())).css_classes(["flat"]).halign(gtk4::Align::Start).build();
        let holder = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        holder.append(&btn);
        let (me, md, holder2, btn2) = (self.clone(), md.clone(), holder.clone(), btn.clone());
        btn.connect_clicked(move |_| {
            btn2.set_sensitive(false);
            btn2.set_label(&format!("⏳ {}", md.label()));
            let (tx, rx) = async_channel::bounded(1);
            let (c, id, key) = (me.client.clone(), md.id.clone(), md.key.clone());
            crate::rt::spawn(async move { let _ = tx.send(c.download_media(&id, &key).await).await; });
            let (me, md, holder2, btn2) = (me.clone(), md.clone(), holder2.clone(), btn2.clone());
            glib::spawn_future_local(async move {
                match rx.recv().await {
                    Ok(Ok(bytes)) => {
                        if md.is_image() {
                            match gtk4::gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)) {
                                Ok(tex) => {
                                    let pic = gtk4::Picture::for_paintable(&tex);
                                    pic.set_can_shrink(true);
                                    pic.set_content_fit(gtk4::ContentFit::ScaleDown);
                                    pic.set_size_request(-1, 240);
                                    pic.set_halign(gtk4::Align::Start);
                                    holder2.remove(&btn2);
                                    holder2.append(&pic);
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
