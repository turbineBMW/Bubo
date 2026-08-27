//! libadwaita UI. All state lives on the GTK thread; the protocol client runs on tokio
//! and reports back through an async-channel drained by a glib-local future.
mod chats;
mod login;
mod state;

use crate::gm;
use adw::prelude::*;
use gtk4::glib;
use std::rc::Rc;

pub fn build(app: &adw::Application) {
    if let Some(display) = gtk4::gdk::Display::default() { crate::accent::install_fallback(&display); }
    let win = adw::ApplicationWindow::builder().application(app).title("Bubo").default_width(1000).default_height(700).build();
    let stack = gtk4::Stack::builder().transition_type(gtk4::StackTransitionType::Crossfade).build();
    win.set_content(Some(&stack));
    win.present();

    match gm::auth::AuthData::load().ok().flatten().filter(|a| a.is_paired()) {
        Some(auth) => start_paired(&win, &stack, auth),
        None => start_pairing(&win, &stack),
    }
}

fn start_pairing(win: &adw::ApplicationWindow, stack: &gtk4::Stack) {
    let (client, events) = match gm::client::Client::new(gm::auth::AuthData::new()) { Ok(x) => x, Err(e) => { fatal(stack, &format!("{e:#}")); return; } };
    let emoji_page = Rc::new(login::EmojiPage::new());
    stack.add_named(&emoji_page.widget, Some("emoji"));
    let (ptx, prx) = async_channel::unbounded::<login::PairProgress>();
    let login_page = login::LoginPage::new({
        let (client, stack, ptx) = (client.clone(), stack.clone(), ptx.clone());
        move |cookies| {
            tracing::info!("got Google session cookies");
            client.auth.lock().unwrap().cookies = cookies;
            stack.set_visible_child_name("emoji");
            // Tear the WebView down — but not from inside its own callback, or WebKit segfaults.
            let stack2 = stack.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || { if let Some(w) = stack2.child_by_name("login") { stack2.remove(&w); } });
            login::run_gaia_pairing(client.clone(), ptx.clone());
        }
    });
    stack.add_named(&login_page.widget, Some("login"));
    stack.set_visible_child_name("login");
    let (win, stack) = (win.clone(), stack.clone());
    let ep = emoji_page.clone();
    glib::spawn_future_local(async move {
        while let Ok(p) = prx.recv().await {
            match p {
                login::PairProgress::Emoji(e) => ep.show_emoji(&e),
                login::PairProgress::Done => ep.paired(),
                login::PairProgress::Failed(e) => ep.error(&e),
            }
        }
    });
    glib::spawn_future_local(async move {
        while let Ok(ev) = events.recv().await {
            match ev {
                gm::events::Event::Connected => { show_chats(&win, &stack, client.clone(), events.clone()); return; }
                gm::events::Event::ListenFatal(e) => emoji_page.error(&e),
                _ => {}
            }
        }
    });
}

fn start_paired(win: &adw::ApplicationWindow, stack: &gtk4::Stack, auth: gm::auth::AuthData) {
    let (client, events) = match gm::client::Client::new(auth) { Ok(x) => x, Err(e) => { fatal(stack, &format!("{e:#}")); return; } };
    let c = client.clone();
    let (tx, rx) = async_channel::bounded::<anyhow::Result<()>>(1);
    crate::rt::spawn(async move { let _ = tx.send(c.connect().await).await; });
    let spinner = adw::StatusPage::builder().title("Connecting to your phone…").build();
    spinner.set_child(Some(&adw::Spinner::new()));
    stack.add_named(&spinner, Some("connecting"));
    stack.set_visible_child_name("connecting");
    let (win, stack) = (win.clone(), stack.clone());
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(())) => show_chats(&win, &stack, client, events),
            Ok(Err(e)) => fatal(&stack, &format!("Could not connect: {e:#}\n\nIf the phone unpaired this device, run `bubo unpair` and pair again.")),
            Err(_) => {}
        }
    });
}

fn show_chats(win: &adw::ApplicationWindow, stack: &gtk4::Stack, client: std::sync::Arc<gm::client::Client>, events: async_channel::Receiver<gm::events::Event>) {
    let view = Rc::new(chats::ChatsView::new(win, client, events));
    stack.add_named(&view.widget, Some("chats"));
    stack.set_visible_child_name("chats");
    view.start();
}

fn fatal(stack: &gtk4::Stack, msg: &str) {
    let p = adw::StatusPage::builder().title("Something went wrong").description(msg).icon_name("dialog-error-symbolic").build();
    stack.add_named(&p, Some("fatal"));
    stack.set_visible_child_name("fatal");
}
