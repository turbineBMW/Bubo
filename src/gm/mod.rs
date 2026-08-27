//! `gm` — a clean-room Rust implementation of the Google Messages for Web protocol
//! ("Bugle"/"Tachyon"). The phone is the server; Google only relays encrypted blobs.
pub mod auth;
pub mod client;
pub mod crypto;
pub mod events;
pub mod gaia;
pub mod http;
pub mod pblite;
pub mod proto;
pub mod session;
