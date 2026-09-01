# Bubo

A native GTK4/libadwaita client for **Google Messages** on Linux. Named after the
mechanical owl from *Clash of the Titans* — a small brass messenger that does what
the gods won't.

It speaks the same protocol as messages.google.com/web: your phone is the server and
Google's relay only sees AES-encrypted blobs. Pairing is the Google-account flow —
sign in, then confirm a matching emoji on the phone — with the session keys coming
out of a UKEY2 handshake between Bubo and the phone. (Google retired QR pairing in
2026; `bubo pair` keeps the QR path for phones that still offer it.)

## Layout

- `proto/`          — wire-format definitions (from mautrix-gmessages' `gmproto`; see Licence)
- `src/gm/`         — clean-room Rust implementation of the protocol
  - `pblite.rs`     — the JSON-array protobuf encoding Google's grpc-web endpoints use
  - `crypto.rs`     — AES-256-CTR + HMAC-SHA256 payload crypto; P-256 refresh signing
  - `auth.rs`       — pairing state (`~/.config/bubo/auth.json`)
  - `client.rs`     — token refresh, long-poll stream, typed RPCs, QR pairing
  - `gaia.rs`       — Google-account pairing: SignInGaia + UKEY2 (P-256 ECDH, HKDF) + emoji
  - `session.rs`    — request/response correlation and message acks
- `src/ui/`         — libadwaita UI: WebKit Google sign-in, emoji page, chat list, thread, composer, attachments

## Running

```sh
cargo run                    # GUI; first launch opens Google sign-in, then shows the emoji
cargo run -- login           # headless: paste a Cookie header from a signed-in messages.google.com tab
cargo run -- pair            # legacy QR pairing in the terminal
cargo run -- probe           # list conversations, then tail live events
cargo run -- probe CONV_ID   # dump recent messages of one conversation
cargo run -- send CONV_ID hello there
cargo run -- unpair
RUST_LOG=bubo=debug cargo run -- probe   # see every RPC
```

When the emoji appears, the phone shows a notification from Messages asking you to
confirm a new device — pick the matching emoji there.

Every so often the phone expires a Google-account pairing (the web client asks you to
pick an emoji again). Bubo does the same: it keeps the Google cookies, drops the pairing
and goes straight back to the emoji page — no sign-in needed unless the cookies are dead too.

## How the protocol works

Google-account (emoji) pairing and message sync verified live end-to-end 2026-08-27.

1. Google-account pairing: with the browser's cookies (`SAPISID` → `SAPISIDHASH`
   Authorization header), `GET /web/config` gives a device ID, then
   `Registration/SignInGaia` (pblite) returns a tachyon token, our browser device and
   the list of phones on the account. Open the stream, then
   `CREATE_GAIA_PAIRING_CLIENT_INIT` / `_CLIENT_FINISHED` carry a UKEY2 handshake
   (unencrypted, TTL 300 s, message type GAIA_2 / BUGLE). The emoji is
   `HKDF(sha256(ECDH), "UKEY2 v1 auth", init‖serverInit)[0..4] mod len(table)`;
   session keys come from `"UKEY2 v1 next"` → client/server keys → (v1) sorted by
   Java `Arrays.hashCode`, sha256'd, HKDF'd with `Ditto salt 1/2`.
   Google-account sessions use the `clients6.google.com` endpoints, network `GDitto`,
   and set `destRegistrationIDs` on every send.
1b. Legacy QR: `Pairing/RegisterPhoneRelay` (binary protobuf) with our P-256 public
   key → tachyon token + pairing key. QR = `support.google.com/messages/?p=web_computer#?c=` +
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

- [x] Google-account (emoji) pairing, legacy QR pairing, token refresh, stream, acks, liveness pings
- [x] Conversation list, message history, send text, read receipts, typing
- [x] Desktop notifications (click to open the conversation)
- [x] Media: view inbound images inline, download files, send images/files (AES-GCM chunked)
- [ ] Reactions, replies, contacts / new conversation UI

## Licence

Bubo's code is MIT. The `.proto` files describe Google's wire format and were taken
from [mautrix-gmessages](https://github.com/mautrix/gmessages) (AGPL-3.0); the Rust
in `src/gm/` was written from the protocol description, not translated from Go.
If you consider the proto files copyrightable expression, treat `proto/` as AGPL.

## Login gotchas (learned the hard way)

- **OSID is mandatory.** messages.google.com/web/config authenticates with the origin-scoped
  `OSID` cookie, which Google only sets *while the /web app page loads* (an accounts→messages
  redirect). Harvesting cookies at the sign-in redirect gives `SAPISID` but not `OSID`, and
  config then 401s to `ServiceLogin?...osid=1`. So we let /web load and poll the jar until both
  `SAPISID` and `OSID` are present.
- **The live web app is a rival pairing client.** Once messages/web boots, its JS starts its own
  UKEY2 handshake. If ours is in flight at the same time, the phone reports `NOT_LATEST_ATTEMPT`
  (error 13/10) and shows the web app's emoji, not ours. Fix: fully tear down the WebView, then
  start our pairing so ours is the latest attempt.
- **Never touch WebKit widgets from inside their own callbacks** (cookie/policy/load) — destroying
  the WebView there segfaults. Defer with `idle_add_local_once` / `timeout_add_local_once`.
- **Long-poll framing:** the body is `[[` frame `,` frame … `]]` where each frame is itself a JSON
  array; skip *both* opening brackets, and never wait for the outer array to close.
