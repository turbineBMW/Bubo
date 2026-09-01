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

    match gm::auth::AuthData::load().ok().flatten() {
        Some(auth) if auth.is_paired() => start_paired(&win, &stack, auth),
        // Cookies but no pairing: the phone expired the last session — straight to the emoji.
        Some(auth) if auth.has_cookies() => start_pairing(&win, &stack, auth),
        _ => start_pairing(&win, &stack, gm::auth::AuthData::new()),
    }
}

/// Drop a page from the stack if it is there (pages are re-created on every (re-)pair).
fn drop_page(stack: &gtk4::Stack, name: &str) { if let Some(w) = stack.child_by_name(name) { stack.remove(&w); } }

/// Google-account pairing. With cookies already in `auth` (a re-pair after the phone expired the
/// session) this goes straight to the emoji, exactly like messages.google.com does, and only
/// falls back to the sign-in WebView if those cookies turn out to be dead.
fn start_pairing(win: &adw::ApplicationWindow, stack: &gtk4::Stack, auth: gm::auth::AuthData) {
    let have_cookies = auth.has_cookies();
    let (client, events) = match gm::client::Client::new(auth) { Ok(x) => x, Err(e) => { fatal(stack, &format!("{e:#}")); return; } };
    drop_page(stack, "emoji");
    let emoji_page = Rc::new(login::EmojiPage::new());
    stack.add_named(&emoji_page.widget, Some("emoji"));
    let (ptx, prx) = async_channel::unbounded::<login::PairProgress>();

    // The sign-in WebView, built only when needed (it is heavy, and usually not needed on a re-pair).
    let show_login: Rc<dyn Fn()> = {
        let (client, stack, ptx) = (client.clone(), stack.clone(), ptx.clone());
        Rc::new(move || {
            drop_page(&stack, "login");
            let login_page = login::LoginPage::new({
                let (client, stack, ptx) = (client.clone(), stack.clone(), ptx.clone());
                move |cookies| {
                    tracing::info!("got Google session cookies");
                    client.auth.lock().unwrap().cookies = cookies;
                    stack.set_visible_child_name("emoji");
                    // Tear the WebView down — but not from inside its own callback, or WebKit segfaults.
                    let stack2 = stack.clone();
                    glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || drop_page(&stack2, "login"));
                    login::run_gaia_pairing(client.clone(), ptx.clone());
                }
            });
            stack.add_named(&login_page.widget, Some("login"));
            stack.set_visible_child_name("login");
        })
    };

    if have_cookies {
        tracing::info!("re-pairing with saved Google cookies");
        emoji_page.signing_in();
        stack.set_visible_child_name("emoji");
        login::run_gaia_pairing(client.clone(), ptx.clone());
    } else {
        show_login();
    }

    let ep = emoji_page.clone();
    let (client2, ptx2, show_login2) = (client.clone(), ptx.clone(), show_login.clone());
    glib::spawn_future_local(async move {
        // Whether this attempt got as far as showing an emoji: a failure before that with saved
        // cookies means the cookies are dead, so fall back to the WebView.
        let mut saw_emoji = false;
        let mut used_saved_cookies = have_cookies;
        while let Ok(p) = prx.recv().await {
            match p {
                login::PairProgress::Emoji(e) => { saw_emoji = true; ep.show_emoji(&e); }
                login::PairProgress::Done => ep.paired(),
                login::PairProgress::Failed(e) => {
                    // Only Google rejecting the cookies means sign in again; the phone not
                    // answering, or a wrong emoji, is worth a plain retry with the same cookies.
                    let cookies_dead = e.contains("HTTP 401") || e.contains("HTTP 403") || e.contains("cookies");
                    if used_saved_cookies && !saw_emoji && cookies_dead {
                        tracing::warn!("re-pair with saved cookies failed ({e}); falling back to sign-in");
                        used_saved_cookies = false;
                        show_login2();
                    } else {
                        let (client, ptx, ep2) = (client2.clone(), ptx2.clone(), ep.clone());
                        saw_emoji = false;
                        ep.error_with_retry(&e, move || { ep2.signing_in(); login::run_gaia_pairing(client.clone(), ptx.clone()); });
                    }
                }
            }
        }
    });
    let (win, stack) = (win.clone(), stack.clone());
    glib::spawn_future_local(async move {
        while let Ok(ev) = events.recv().await {
            match ev {
                gm::events::Event::Connected => { drop_page(&stack, "emoji"); show_chats(&win, &stack, client.clone(), events.clone()); return; }
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
            Ok(Ok(())) => { drop_page(&stack, "connecting"); show_chats(&win, &stack, client, events); }
            Ok(Err(e)) => {
                let p = fatal(&stack, &format!("Could not connect: {e:#}"));
                let b = gtk4::Button::builder().label("Pair again").css_classes(["pill", "suggested-action"]).halign(gtk4::Align::Center).build();
                let (win, stack, client) = (win.clone(), stack.clone(), client.clone());
                b.connect_clicked(move |_| repair(&win, &stack, &client));
                p.set_child(Some(&b));
            }
            Err(_) => {}
        }
    });
}

/// The phone expired the pairing (or it is otherwise dead): keep the Google cookies, forget the
/// rest, and run the emoji pairing again on a fresh client.
fn repair(win: &adw::ApplicationWindow, stack: &gtk4::Stack, client: &std::sync::Arc<gm::client::Client>) {
    client.disconnect();
    let auth = client.auth.lock().unwrap().for_repair();
    // Persist the stripped state so a relaunch mid-way also lands on the emoji, not the old session.
    if let Err(e) = auth.save() { tracing::warn!("saving auth for re-pair: {e:#}"); }
    for p in ["chats", "fatal", "connecting"] { drop_page(stack, p); }
    start_pairing(win, stack, auth);
}

fn show_chats(win: &adw::ApplicationWindow, stack: &gtk4::Stack, client: std::sync::Arc<gm::client::Client>, events: async_channel::Receiver<gm::events::Event>) {
    let view = Rc::new(chats::ChatsView::new(win, client.clone(), events));
    stack.add_named(&view.widget, Some("chats"));
    stack.set_visible_child_name("chats");
    {
        let (win, stack) = (win.clone(), stack.clone());
        view.set_on_session_expired(move || repair(&win, &stack, &client));
    }
    view.start();
}

fn fatal(stack: &gtk4::Stack, msg: &str) -> adw::StatusPage {
    drop_page(stack, "fatal");
    let p = adw::StatusPage::builder().title("Something went wrong").description(msg).icon_name("dialog-error-symbolic").build();
    stack.add_named(&p, Some("fatal"));
    stack.set_visible_child_name("fatal");
    p
}
