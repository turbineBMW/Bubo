//! libadwaita UI. All state lives on the GTK thread; the protocol client runs on tokio
//! and reports back through an async-channel drained by a glib-local future.
mod chats;
mod pair;
mod state;

use crate::gm;
use adw::prelude::*;
use gtk4::glib;
use std::rc::Rc;

pub fn build(app: &adw::Application) {
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
    let page = pair::PairPage::new(client.clone());
    stack.add_named(&page.widget, Some("pair"));
    stack.set_visible_child_name("pair");
    let (win, stack) = (win.clone(), stack.clone());
    glib::spawn_future_local(async move {
        while let Ok(ev) = events.recv().await {
            match ev {
                gm::events::Event::Paired { .. } => { page.paired(); }
                gm::events::Event::Connected => { show_chats(&win, &stack, client.clone(), events.clone()); return; }
                gm::events::Event::ListenFatal(e) => page.error(&e),
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
