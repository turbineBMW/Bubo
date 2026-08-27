//! First-run page: shows the QR code the phone scans.
use crate::gm::client::Client;
use adw::prelude::*;
use gtk4::{gdk, glib};
use std::sync::Arc;

pub struct PairPage { pub widget: adw::StatusPage, picture: gtk4::Picture, client: Arc<Client> }

impl PairPage {
    pub fn new(client: Arc<Client>) -> Self {
        let picture = gtk4::Picture::builder().width_request(280).height_request(280).halign(gtk4::Align::Center).build();
        let frame = gtk4::Frame::builder().child(&picture).halign(gtk4::Align::Center).build();
        frame.add_css_class("card");
        let widget = adw::StatusPage::builder().title("Pair with your phone")
            .description("Open Messages on your phone → ⋮ → Device pairing → QR code scanner.").build();
        widget.set_child(Some(&frame));
        let page = Self { widget, picture, client };
        page.refresh(true);
        page
    }

    /// Fetch (or re-fetch) a pairing key and draw it. The relay key expires, so refresh every ~5 min.
    fn refresh(&self, first: bool) {
        let c = self.client.clone();
        let (tx, rx) = async_channel::bounded::<anyhow::Result<String>>(1);
        crate::rt::spawn(async move { let r = if first { c.start_pairing().await } else { c.refresh_pairing().await }; let _ = tx.send(r).await; });
        let pic = self.picture.clone();
        let page = self.widget.clone();
        let client = self.client.clone();
        glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok(Ok(url)) => {
                    pic.set_paintable(Some(&qr_texture(&url)));
                    let pic2 = pic.clone(); let page2 = page.clone();
                    glib::timeout_add_local_once(std::time::Duration::from_secs(300), move || {
                        if pic2.is_mapped() { PairPage { widget: page2, picture: pic2, client }.refresh(false); }
                    });
                }
                Ok(Err(e)) => page.set_description(Some(&format!("Could not reach Google's relay: {e:#}"))),
                Err(_) => {}
            }
        });
    }

    pub fn paired(&self) {
        self.widget.set_title("Paired!");
        self.widget.set_description(Some("Connecting…"));
        self.picture.set_paintable(None::<&gdk::Paintable>);
    }
    pub fn error(&self, e: &str) { self.widget.set_description(Some(e)); }
}

fn qr_texture(data: &str) -> gdk::MemoryTexture {
    let code = qrcode::QrCode::new(data.as_bytes()).expect("qr");
    let w = code.width();
    let scale = 6usize; let quiet = 4usize;
    let side = (w + quiet * 2) * scale;
    let mut px = vec![255u8; side * side * 4];
    for y in 0..w { for x in 0..w {
        if code[(x, y)] == qrcode::Color::Dark {
            for dy in 0..scale { for dx in 0..scale {
                let i = (((y + quiet) * scale + dy) * side + (x + quiet) * scale + dx) * 4;
                px[i] = 0; px[i + 1] = 0; px[i + 2] = 0;
            } }
        }
    } }
    gdk::MemoryTexture::new(side as i32, side as i32, gdk::MemoryFormat::R8g8b8a8, &glib::Bytes::from_owned(px), side * 4)
}
