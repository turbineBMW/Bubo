//! Google sign-in in a WebView. Once messages.google.com has a SAPISID cookie we own a
//! signed-in session; hand the cookies to the client and run the emoji pairing.
use crate::gm::client::Client;
use adw::prelude::*;
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
        web.connect_load_changed(move |wv, ev| {
            if ev != webkit6::LoadEvent::Finished || done.get() { return; }
            let Some(uri) = wv.uri() else { return };
            if !uri.starts_with("https://messages.google.com/") { return; }
            let mgr = wv.network_session().unwrap().cookie_manager().unwrap();
            let (done, on_cookies) = (done.clone(), on_cookies.clone());
            mgr.cookies("https://messages.google.com/", None::<&gtk4::gio::Cancellable>, move |res| {
                let Ok(mut cookies) = res else { return };
                let map: std::collections::HashMap<String, String> = cookies.iter_mut().filter_map(|c| Some((c.name()?.to_string(), c.value()?.to_string()))).collect();
                if map.contains_key("SAPISID") && !done.replace(true) { on_cookies(map); }
            });
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
    pub fn error(&self, e: &str) { self.widget.set_title("Pairing failed"); self.widget.set_description(Some(e)); self.widget.set_child(None::<&gtk4::Widget>); }
    pub fn paired(&self) { self.widget.set_title("Paired!"); self.widget.set_description(Some("Connecting…")); self.widget.set_child(None::<&gtk4::Widget>); }
}

/// Run both pairing phases on tokio; results come back on `tx`.
pub enum PairProgress { Emoji(String), Done(String), Failed(String) }

pub fn run_gaia_pairing(client: Arc<Client>, tx: async_channel::Sender<PairProgress>) {
    crate::rt::spawn(async move {
        match client.start_gaia_pairing().await {
            Err(e) => { let _ = tx.send(PairProgress::Failed(format!("{e:#}"))).await; }
            Ok((emoji, ps)) => {
                let _ = tx.send(PairProgress::Emoji(emoji)).await;
                match client.finish_gaia_pairing(ps).await {
                    Ok(id) => { let _ = tx.send(PairProgress::Done(id)).await; }
                    Err(e) => { let _ = tx.send(PairProgress::Failed(format!("{e:#}"))).await; }
                }
            }
        }
    });
}

