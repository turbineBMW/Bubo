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
        // Once sign-in redirects into the web app, take the cookies and refuse the navigation:
        // the real web client would otherwise start its own pairing handshake and supersede ours.
        // Ask the cookie jar for messages.google.com; `f` gets the map if it has a SAPISID (= signed in).
        let with_session = |wv: &webkit6::WebView, f: Box<dyn FnOnce(Option<std::collections::HashMap<String, String>>)>| {
            let mgr = wv.network_session().unwrap().cookie_manager().unwrap();
            mgr.cookies("https://messages.google.com/", None::<&gtk4::gio::Cancellable>, move |res| {
                let map: std::collections::HashMap<String, String> = res.ok().map(|mut c| c.iter_mut().filter_map(|c| Some((c.name()?.to_string(), c.value()?.to_string()))).collect()).unwrap_or_default();
                f(if map.contains_key("SAPISID") { Some(map) } else { None });
            });
        };
        let finish = {
            let (done, on_cookies) = (done.clone(), on_cookies.clone());
            move |wv: &webkit6::WebView, map: std::collections::HashMap<String, String>| {
                if done.replace(true) { return; }
                let wv = wv.clone();
                // Leave WebKit's callback first; then blank the view and hand the cookies over.
                gtk4::glib::idle_add_local_once(move || { wv.stop_loading(); wv.load_uri("about:blank"); on_cookies(map); });
            }
        };
        let is_app_url = |u: &str| u.starts_with("https://messages.google.com/web") && !u.starts_with("https://messages.google.com/web/authentication");
        // Once signed in, the app URL is where the real web client would boot and start its own
        // pairing handshake (which supersedes ours). Decide asynchronously: signed in → take the
        // cookies and refuse the navigation; not yet → let it through (that's the sign-in hop).
        let (f, d) = (finish.clone(), done.clone());
        web.connect_decide_policy(move |wv, decision, kind| {
            if kind != webkit6::PolicyDecisionType::NavigationAction || d.get() { return false; }
            let Some(nav) = decision.downcast_ref::<webkit6::NavigationPolicyDecision>() else { return false };
            let Some(uri) = nav.navigation_action().and_then(|a| a.request()).and_then(|r| r.uri()) else { return false };
            if !is_app_url(&uri) { return false; }
            let (decision, wv, f) = (decision.clone(), wv.clone(), f.clone());
            with_session(&wv.clone(), Box::new(move |map| match map {
                Some(map) => { decision.ignore(); f(&wv, map); }
                None => { decision.use_(); }
            }));
            true
        });
        // Belt and braces: if the app page starts loading anyway, harvest at commit time, before its JS runs far.
        web.connect_load_changed(move |wv, ev| {
            if !matches!(ev, webkit6::LoadEvent::Committed | webkit6::LoadEvent::Finished) { return; }
            if !wv.uri().map(|u| is_app_url(&u)).unwrap_or(false) { return; }
            let (wv2, f) = (wv.clone(), finish.clone());
            with_session(wv, Box::new(move |map| if let Some(map) = map { f(&wv2, map) }));
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

