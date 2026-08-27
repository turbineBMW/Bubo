# Bubo

A native GTK4/libadwaita client for **Google Messages** on Linux. Named after the
mechanical owl from *Clash of the Titans* — a small brass messenger that does what
the gods won't.

It speaks the same protocol as messages.google.com/web: your phone is the server,
Google's relay only sees AES-encrypted blobs, and the keys never leave the QR code.
No Google account cookies are needed for QR pairing.

## Layout

- `proto/`          — wire-format definitions (from mautrix-gmessages' `gmproto`; see Licence)
- `src/gm/`         — clean-room Rust implementation of the protocol
  - `pblite.rs`     — the JSON-array protobuf encoding Google's grpc-web endpoints use
  - `crypto.rs`     — AES-256-CTR + HMAC-SHA256 payload crypto; P-256 refresh signing
  - `auth.rs`       — pairing state (`~/.config/bubo/auth.json`)
  - `client.rs`     — pairing, token refresh, long-poll stream, typed RPCs
  - `session.rs`    — request/response correlation and message acks
- `src/ui/`         — libadwaita UI: QR pairing page, chat list, thread, composer

## Running

```sh
cargo run                    # GUI; first launch shows the QR code to scan
cargo run -- pair            # headless pairing: QR in the terminal
cargo run -- probe           # list conversations, then tail live events
cargo run -- probe CONV_ID   # dump recent messages of one conversation
cargo run -- send CONV_ID hello there
cargo run -- unpair
RUST_LOG=bubo=debug cargo run -- probe   # see every RPC
```

Pairing on the phone: **Messages → ⋮ → Device pairing → QR code scanner**.

## How the protocol works (verified 2026-08-27)

1. `Pairing/RegisterPhoneRelay` (binary protobuf) with our P-256 public key → tachyon
   token + pairing key. QR = `support.google.com/messages/?p=web_computer#?c=` +
   base64(`URLData{pairingKey, aesKey, hmacKey}`).
2. `Messaging/ReceiveMessages` (pblite) is a long-lived HTTP stream: `[[` then
   comma-separated pblite arrays (`LongPollingPayload`) then `]]`. The phone's
   `PairEvent/Paired` arrives here after the scan, carrying the browser/mobile
   device identities and a fresh token.
3. Every RPC is `Messaging/SendMessage` (pblite) wrapping an `OutgoingRPCData`
   whose payload is AES-CTR encrypted; the **reply comes back on the stream** with
   `sessionID == our requestID`. Each received frame must be acked
   (`Messaging/AckMessages`, batched every 5 s) or delivery stalls.
4. Tokens expire in ~24 h; `Registration/RegisterRefresh` is signed with the P-256
   key over SHA-256(`requestID:timestampµs`).
5. On (re)connect send `GET_UPDATES` with a fresh session ID — that's what makes
   the phone push events to *this* session. The stream's opening `ack{count}`
   says how many replayed events to treat as old.

## Status

- [x] QR pairing, token refresh, stream, acks, phone-liveness pings
- [x] Conversation list, message history, send text, read receipts, typing
- [ ] Media (download/upload — needs the AES-GCM chunked scheme in `media.go`)
- [ ] Reactions, replies, contacts / new conversation UI
- [ ] Desktop notifications
- [ ] Google-account pairing (no QR; needs Gaia cookies + UKEY2 handshake)

## Licence

Bubo's code is MIT. The `.proto` files describe Google's wire format and were taken
from [mautrix-gmessages](https://github.com/mautrix/gmessages) (AGPL-3.0); the Rust
in `src/gm/` was written from the protocol description, not translated from Go.
If you consider the proto files copyrightable expression, treat `proto/` as AGPL.
