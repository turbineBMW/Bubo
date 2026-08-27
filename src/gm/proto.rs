//! Generated protobuf types. One module per proto package.
#![allow(clippy::all, non_camel_case_types, dead_code)]
pub const FILE_DESCRIPTOR_SET: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/file_descriptor_set.bin"));
pub mod authentication { include!(concat!(env!("OUT_DIR"), "/authentication.rs")); }
pub mod client { include!(concat!(env!("OUT_DIR"), "/client.rs")); }
pub mod config { include!(concat!(env!("OUT_DIR"), "/config.rs")); }
pub mod conversations { include!(concat!(env!("OUT_DIR"), "/conversations.rs")); }
pub mod events { include!(concat!(env!("OUT_DIR"), "/events.rs")); }
pub mod rpc { include!(concat!(env!("OUT_DIR"), "/rpc.rs")); }
pub mod settings { include!(concat!(env!("OUT_DIR"), "/settings.rs")); }
pub mod ukey { include!(concat!(env!("OUT_DIR"), "/ukey.rs")); }
pub mod util { include!(concat!(env!("OUT_DIR"), "/util.rs")); }
