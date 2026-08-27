//! Google sign-in in a WebView. Once messages.google.com has a SAPISID cookie we own a
//! signed-in session; hand the cookies to the client and run the emoji pairing.
use crate::gm::client::Client;
#[allow(unused_imports)] use adw::prelude::*;
#[allow(unused_imports)] use gtk4::prelude::*;

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use webkit6::prelude::*;

const START_URL: &str = "https://messages.google.com/web/authentication";

pub struct LoginPage { pub widget: adw::NavigationPage }

impl LoginPage {
    /// `on_cookies` fires once with the harvested cookies.
    pub fn new(on_cookies: impl Fn(std::collections::HashMap<String, String>) + 'static) -> Self {
        let dirs = directories::ProjectDirs::from("dev", "turbinebmw", "bubo").unwrap();
        let session = webkit6::NetworkSession::new(Some(dirs.data_dir().join("webkit").to_str().unwrap()), Some(dirs.cache_dir().join("webkit").to_str().unwrap()));
        let web = webkit6::WebView::builder().network_session(&session).vexpand(true).build();
        if let Some(s) = WebViewExt::settings(&web) { s.set_user_agent(Some(crate::gm::http::USER_AGENT)); }
        let header = adw::HeaderBar::builder().title_widget(&adw::WindowTitle::new("Sign in to Google", "Bubo never sees your password — only the session cookies")).build();
        let tv = adw::ToolbarView::builder().content(&web).build();
        tv.add_top_bar(&header);
        let widget = adw::NavigationPage::builder().title("Sign in").child(&tv).build();

        let done = Rc::new(Cell::new(false));
        let on_cookies = Rc::new(on_cookies);
        // messages.google.com authenticates with an OSID cookie that is only issued while the
        // /web app page loads, so we must let that page load. We poll the cookie jar until both
        // SAPISID and OSID are present, then harvest and tear the WebView down — starting our own
        // pairing only afterwards, so the web app's rival handshake can't supersede ours.
        let is_app_url = |u: &str| u.starts_with("https://messages.google.com/web") && !u.starts_with("https://messages.google.com/web/authentication");
        fn try_harvest(wv: &webkit6::WebView, done: Rc<Cell<bool>>, on_cookies: Rc<dyn Fn(std::collections::HashMap<String, String>)>, attempt: u32) {
            if done.get() { return; }
            let mgr = wv.network_session().unwrap().cookie_manager().unwrap();
            let (wv, done, on_cookies) = (wv.clone(), done.clone(), on_cookies.clone());
            mgr.cookies("https://messages.google.com/", None::<&gtk4::gio::Cancellable>, move |res| {
                if done.get() { return; }
                let map: std::collections::HashMap<String, String> = res.ok().map(|mut c| c.iter_mut().filter_map(|c| Some((c.name()?.to_string(), c.value()?.to_string()))).collect()).unwrap_or_default();
                let ready = map.contains_key("SAPISID") && map.contains_key("OSID");
                if ready || (attempt >= 25 && map.contains_key("SAPISID")) {
                    if done.replace(true) { return; }
                    if !map.contains_key("OSID") { tracing::warn!("proceeding without OSID cookie; /web/config may 401"); }
                    let (wv, on_cookies) = (wv.clone(), on_cookies.clone());
                    gtk4::glib::idle_add_local_once(move || { wv.stop_loading(); wv.load_uri("about:blank"); on_cookies(map); });
                } else {
                    gtk4::glib::timeout_add_local_once(std::time::Duration::from_millis(400), move || try_harvest(&wv, done, on_cookies, attempt + 1));
                }
            });
        }
        web.connect_load_changed(move |wv, ev| {
            if !matches!(ev, webkit6::LoadEvent::Committed | webkit6::LoadEvent::Finished) { return; }
            if wv.uri().map(|u| is_app_url(&u)).unwrap_or(false) { try_harvest(wv, done.clone(), on_cookies.clone(), 0); }
        });
        web.load_uri(START_URL);
        Self { widget }
    }
}

pub struct EmojiPage { pub widget: adw::StatusPage }

impl EmojiPage {
    pub fn new() -> Self {
        let widget = adw::StatusPage::builder().title("Signing in…").icon_name("phone-symbolic").build();
        Self { widget }
    }
    pub fn show_emoji(&self, emoji: &str) {
        let l = gtk4::Label::builder().label(emoji).css_classes(["bubo-emoji"]).build();
        let css = gtk4::CssProvider::new();
        css.load_from_string(".bubo-emoji { font-size: 96px; }");
        gtk4::style_context_add_provider_for_display(&gtk4::gdk::Display::default().unwrap(), &css, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
        self.widget.set_title("Tap this emoji on your phone");
        self.widget.set_description(Some("Messages on the phone is asking you to confirm a new device.\nPick the emoji that matches this one."));
        self.widget.set_child(Some(&l));
    }
    pub fn error(&self, e: &str) { self.widget.set_title("Pairing failed"); self.widget.set_description(Some(&gtk4::glib::markup_escape_text(e))); self.widget.set_child(None::<&gtk4::Widget>); }
    pub fn paired(&self) { self.widget.set_title("Paired!"); self.widget.set_description(Some("Connecting…")); self.widget.set_child(None::<&gtk4::Widget>); }
}

/// Run both pairing phases on tokio; results come back on `tx`.
pub enum PairProgress { Emoji(String), Done, Failed(String) }

pub fn run_gaia_pairing(client: Arc<Client>, tx: async_channel::Sender<PairProgress>) {
    crate::rt::spawn(async move {
        match client.start_gaia_pairing().await {
            Err(e) => { let _ = tx.send(PairProgress::Failed(format!("{e:#}"))).await; }
            Ok((emoji, ps)) => {
                let _ = tx.send(PairProgress::Emoji(emoji)).await;
                match client.finish_gaia_pairing(ps).await {
                    Ok(_) => { let _ = tx.send(PairProgress::Done).await; }
                    Err(e) => { let _ = tx.send(PairProgress::Failed(format!("{e:#}"))).await; }
                }
            }
        }
    });
}

