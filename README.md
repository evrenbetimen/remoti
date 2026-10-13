# Remoti

P2P remote desktop with NAT traversal and end-to-end encryption.
Rust core, Rust/Axum signaling service, and an Electron + React shell.

```
remoti/
├── Cargo.toml                 workspace (resolver 3, edition 2024, LTO release profile)
├── crates/
│   ├── proto/                 wire types shared by every component (serde/postcard, STUN codec)
│   └── core_engine/           remoti_core lib + remoti-agent binary
│       └── src/
│           ├── main.rs        CLI: ipc (Electron) | host | connect
│           ├── agent.rs       signaling events → host/viewer session supervision
│           ├── handshake.rs   X25519 + SPAKE2 handshake, HKDF, key confirmation
│           ├── crypto.rs      AES-256-GCM record layer + replay window
│           ├── network.rs     quinn QUIC, STUN gathering, hole punching, cert pinning
│           ├── relay.rs       TCP/WebSocket fallback through the blind relay
│           ├── transport.rs   transport-agnostic SecureLink
│           ├── congestion.rs  bitrate/FPS controller
│           ├── capture/       DXGI (Windows), CoreGraphics (macOS), synthetic
│           ├── frame.rs       delta-frame tile tracker
│           ├── encoder.rs     OpenH264 low-latency encoder
│           ├── input/         sanitizer, SendInput (Windows), CGEvent (macOS)
│           ├── host.rs viewer.rs stream.rs router.rs signaling.rs identity.rs
│           └── ipc.rs         stdio framing to Electron
├── services/signaling/        Axum: /ws signaling, /relay blind relay, UDP STUN responder
└── apps/desktop/              Electron main/preload + React renderer (WebCodecs)
```

## Data path

```
host: capture (DXGI/CG) ─► DeltaTracker ─► H.264 ─► seal(AES-GCM) ─► QUIC uni-stream per frame
                                                                     └► WebSocket relay (UDP blocked)
viewer: open ─► stdio frame (kind=2) ─► Electron main ─► MessagePort ─► WebCodecs VideoDecoder ─► canvas
```

The signaling service sees device ids and opaque handshake payloads only. Video and input never pass through it.
When UDP is blocked, the relay forwards sealed records it cannot decrypt.

## Security model

* **Device identity**: an Ed25519 key per install. The 9-digit id is derived from `SHA-256(pubkey)`, and signaling
  requires proof of possession (challenge signature), so nobody can claim another device's id.
* **Session keys**: each session uses ephemeral X25519 keys plus SPAKE2 over the host's one-time password. The
  transcript hash covers ids, both SPAKE messages, both X25519 keys and the host's QUIC certificate fingerprint.
  HKDF-SHA256 derives per-direction AES-256-GCM keys and HMAC confirmation keys. A malicious signaling server cannot
  MITM the session without knowing the password.
* **Offline guessing resistance**: the viewer must confirm first. The host answers only after verifying, so a failed
  attempt gives an attacker nothing to test guesses against. After 5 failures (including stalled confirmations) the
  host rotates its password.
* **Transport**: QUIC/TLS 1.3 with an ephemeral self-signed certificate pinned through the handshake transcript. The
  AES-GCM record layer runs on top, with a replay window and channel-bound AAD, so the relay path gets identical
  guarantees.
* **Input**: viewer events are validated before injection. The sanitizer rate-limits and clamps coordinates, blocks
  secure-attention chords, strips control and bidi characters from text, and releases held keys when the session ends.

## Running

Requirements: Rust 1.85+ and Node 22+. Building OpenH264 needs a C/C++ toolchain.

```sh
# signaling + STUN + relay (local)
cargo run -p remoti-signaling -- --listen 127.0.0.1:8080 --stun-listen 127.0.0.1:3478

# headless host (synthetic capture works without a display)
cargo run -p remoti-core --bin remoti-agent -- --signaling ws://127.0.0.1:8080/ws host
# headless viewer
cargo run -p remoti-core --bin remoti-agent -- --signaling ws://127.0.0.1:8080/ws connect <ID> --password <PW>

# desktop app (spawns target/{release,debug}/remoti-agent over stdio)
cargo build -p remoti-core --release
cd apps/desktop && npm ci && REMOTI_SIGNALING_URL=ws://127.0.0.1:8080/ws npm run dev
```

Useful agent flags: `--force-relay`, `--capture synthetic`, `--max-bitrate-kbps`, `--max-fps`, `--log-format json`.

## Tests

```sh
cargo test --workspace     # unit tests + in-process e2e (QUIC, relay, wrong password)
cd apps/desktop && npm test && npm run typecheck
```

## Design choices

* **ring** for X25519, AES-256-GCM, HKDF, HMAC and Ed25519, plus `spake2` for the PAKE. No PQC yet; the transcript
  can take an ML-KEM share later without changing the record layer.
* **WebSocket signaling** instead of libp2p. It is simpler to operate behind standard load balancers, and the same
  service hosts the relay.
* **Capture** is cfg-gated: DXGI Desktop Duplication on Windows and CoreGraphics `CGDisplay` capture on macOS
  (ScreenCaptureKit is the planned upgrade). OS dirty rects feed the delta tracker, which also hashes 64×64 tiles so unchanged frames are skipped.
* **Codec**: OpenH264 (Constrained Baseline, Annex B, screen-content mode). H.265 and AV1 are negotiated in the
  protocol and mapped to WebCodecs strings, but not encoded yet.
* **Congestion**: a delay- and loss-based controller fed by QUIC stats and viewer reports, with multiplicative decrease,
  additive increase and application-limited detection. It throttles FPS in steps from 10 to 60.
