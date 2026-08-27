mod accent;
mod gm;
mod rt;
mod ui;

use adw::prelude::*;
use gm::proto::client::list_conversations_request::Folder;

const APP_ID: &str = "dev.turbinebmw.Bubo";

fn main() -> anyhow::Result<()> {
    init_logging();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("pair") => return rt::block_on(cli_pair()),
        Some("login") => return rt::block_on(cli_login()),
        Some("probe") => return rt::block_on(cli_probe(args.get(2).cloned())),
        Some("send") => return rt::block_on(cli_send(args.get(2).cloned().unwrap_or_default(), args[3..].join(" "))),
        Some("unpair") => return rt::block_on(cli_unpair()),
        _ => {}
    }
    let app = adw::Application::builder().application_id(APP_ID).flags(gtk4::gio::ApplicationFlags::NON_UNIQUE).build();
    app.connect_activate(|app| { gtk4::Window::set_default_icon_name(APP_ID); ui::build(app); });
    app.run_with_args::<&str>(&[]);
    Ok(())
}

/// stderr at RUST_LOG (default info) + always a debug log at ~/.cache/bubo/bubo.log.
fn init_logging() {
    use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};
    let stderr = tracing_subscriber::fmt::layer().with_writer(std::io::stderr)
        .with_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "bubo=info".parse().unwrap()));
    let dir = directories::ProjectDirs::from("dev", "turbinebmw", "bubo").map(|d| d.cache_dir().to_path_buf()).unwrap();
    let _ = std::fs::create_dir_all(&dir);
    let file = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("bubo.log")).ok();
    let file_layer = file.map(|f| tracing_subscriber::fmt::layer().with_ansi(false).with_writer(std::sync::Mutex::new(f))
        .with_filter(tracing_subscriber::EnvFilter::new("bubo=debug")));
    tracing_subscriber::registry().with(stderr).with(file_layer).init();
    tracing::info!("bubo {} starting", env!("CARGO_PKG_VERSION"));
}

/// `bubo pair` — headless: prints a QR code to the terminal; scan it with Messages → Device pairing.
async fn cli_pair() -> anyhow::Result<()> {
    let (client, events) = gm::client::Client::new(gm::auth::AuthData::new())?;
    let url = client.start_pairing().await?;
    let qr = qrcode::QrCode::new(url.as_bytes())?;
    println!("{}", qr.render::<qrcode::render::unicode::Dense1x2>().dark_color(qrcode::render::unicode::Dense1x2::Light).light_color(qrcode::render::unicode::Dense1x2::Dark).build());
    println!("\nOn the phone: Messages → ⋮ → Device pairing → QR code scanner. Waiting…");
    loop {
        match events.recv().await? {
            gm::events::Event::Paired { phone_id } => { println!("paired with {phone_id}; saved {}", gm::auth::path().display()); return Ok(()); }
            gm::events::Event::ListenFatal(e) => anyhow::bail!("listen failed: {e}"),
            e => tracing::debug!(?e, "event"),
        }
    }
}

/// `bubo login` — headless Google-account pairing. Paste the `Cookie:` header of a request
/// to messages.google.com from a signed-in browser (DevTools → Network → any request → Request Headers).
async fn cli_login() -> anyhow::Result<()> {
    println!("Paste the Cookie header value from a signed-in messages.google.com tab, then Enter:");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let mut auth = gm::auth::AuthData::new();
    auth.cookies = line.trim().trim_start_matches("Cookie:").trim().split(';').filter_map(|kv| kv.trim().split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
    if !auth.has_cookies() { anyhow::bail!("no SAPISID cookie in that header — are you signed in?"); }
    let (client, events) = gm::client::Client::new(auth)?;
    let (emoji, ps) = client.start_gaia_pairing().await?;
    println!("\n    Tap this emoji on your phone:   {emoji}\n");
    let id = client.finish_gaia_pairing(ps).await?;
    println!("paired with {id}; saved {}", gm::auth::path().display());
    // wait for the reconnect to open so a bad key shows up now rather than on next launch
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(30), events.recv()).await?? {
            gm::events::Event::Connected => { println!("connected"); return Ok(()); }
            gm::events::Event::ListenFatal(e) => anyhow::bail!("{e}"),
            _ => {}
        }
    }
}

async fn connected() -> anyhow::Result<(std::sync::Arc<gm::client::Client>, async_channel::Receiver<gm::events::Event>)> {
    let auth = gm::auth::AuthData::load()?.filter(|a| a.is_paired()).ok_or_else(|| anyhow::anyhow!("not paired; run `bubo pair`"))?;
    let (client, events) = gm::client::Client::new(auth)?;
    client.connect().await?;
    // wait for the stream to open
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(30), events.recv()).await?? {
            gm::events::Event::Connected => break,
            gm::events::Event::ListenFatal(e) => anyhow::bail!("{e}"),
            e => tracing::debug!(?e, "event"),
        }
    }
    Ok((client, events))
}

/// `bubo probe [CONV_ID]` — list conversations, or dump recent messages of one, then tail events.
async fn cli_probe(conv: Option<String>) -> anyhow::Result<()> {
    let (client, events) = connected().await?;
    match conv {
        None => {
            let r = client.list_conversations(25, Folder::Inbox).await?;
            for c in &r.conversations {
                let lm = c.latest_message.as_ref().map(|m| m.display_content.as_str()).unwrap_or("");
                println!("{}  {:<28}  {}{}", c.conversation_id, c.name.chars().take(28).collect::<String>(), if c.unread { "● " } else { "" }, lm.chars().take(60).collect::<String>());
            }
        }
        Some(id) => {
            let r = client.list_messages(&id, 30, None).await?;
            for m in r.messages.iter().rev() {
                let text = m.message_info.iter().filter_map(|i| match &i.data { Some(gm::proto::conversations::message_info::Data::MessageContent(c)) => Some(c.content.clone()), Some(gm::proto::conversations::message_info::Data::MediaContent(mc)) => Some(format!("[media {}]", mc.mime_type)), None => None }).collect::<Vec<_>>().join(" ");
                let who = m.sender_participant.as_ref().map(|p| if p.is_me { "me".to_string() } else { p.full_name.clone() }).unwrap_or_default();
                println!("{}  {:<16} {}", m.timestamp, who, text);
            }
        }
    }
    println!("--- tailing events (Ctrl-C to stop) ---");
    while let Ok(e) = events.recv().await {
        match e {
            gm::events::Event::Message { msg, is_old } => println!("msg {}{}: {:?}", if is_old { "(old) " } else { "" }, msg.conversation_id, msg.message_info.first().and_then(|i| i.data.clone())),
            gm::events::Event::Conversation(c) => println!("conv {} {:?}", c.conversation_id, c.latest_message.as_ref().map(|m| &m.display_content)),
            e => println!("{e:?}"),
        }
    }
    Ok(())
}

async fn cli_send(conv: String, text: String) -> anyhow::Result<()> {
    let (client, _events) = connected().await?;
    let c = client.get_conversation(&conv).await?.ok_or_else(|| anyhow::anyhow!("no such conversation"))?;
    let r = client.send_text(&conv, &c.default_outgoing_id, &text, None).await?;
    println!("status: {:?}", gm::proto::client::send_message_response::Status::try_from(r.status));
    Ok(())
}

async fn cli_unpair() -> anyhow::Result<()> {
    let auth = gm::auth::AuthData::load()?.ok_or_else(|| anyhow::anyhow!("not paired"))?;
    let (client, _) = gm::client::Client::new(auth)?;
    client.unpair().await?;
    std::fs::remove_file(gm::auth::path())?;
    println!("unpaired");
    Ok(())
}
