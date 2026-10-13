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
│           ├── handshake.rs   X25519 + ML-KEM-768 + SPAKE2 hybrid handshake, HKDF, key confirmation
│           ├── crypto.rs      AES-256-GCM record layer + replay window
│           ├── network.rs     quinn QUIC, STUN gathering, hole punching, cert pinning
│           ├── relay.rs       TCP/WebSocket fallback through the blind relay
│           ├── transport.rs   transport-agnostic SecureLink
│           ├── congestion.rs  bitrate/FPS controller
│           ├── capture/       DXGI (Windows), ScreenCaptureKit / CoreGraphics (macOS), synthetic
│           ├── frame.rs       delta-frame tile tracker
│           ├── color.rs       BGRA → I420 / NV12 (BT.709 limited range)
│           ├── encoder/       codec negotiation; OpenH264, rav1e (AV1), Media Foundation, VideoToolbox
│           ├── input/         sanitizer, SendInput (Windows), CGEvent (macOS)
│           ├── host.rs viewer.rs stream.rs router.rs signaling.rs identity.rs
│           └── ipc.rs         stdio framing to Electron
├── services/signaling/        Axum: /ws signaling, /relay blind relay, UDP STUN responder
└── apps/desktop/              Electron main/preload + React renderer (WebCodecs)
```

## Data path

```
host: capture (DXGI/SCK) ─► DeltaTracker ─► H.26x/AV1 ─► seal(AES-GCM) ─► QUIC uni-stream per frame
                                                                     └► WebSocket relay (UDP blocked)
viewer: open ─► stdio frame (kind=2) ─► Electron main ─► MessagePort ─► WebCodecs VideoDecoder ─► canvas
```

The signaling service sees device ids and opaque handshake payloads only. Video and input never pass through it.
When UDP is blocked, the relay forwards sealed records it cannot decrypt.

## Security model

* **Device identity**: an Ed25519 key per install. The 9-digit id is derived from `SHA-256(pubkey)`, and signaling
  requires proof of possession (challenge signature), so nobody can claim another device's id.
* **Session keys**: each session runs a hybrid key exchange. The viewer sends an ephemeral X25519 key and an
  ML-KEM-768 (FIPS 203) encapsulation key; the host answers with its X25519 key and an ML-KEM ciphertext, and both
  sides run SPAKE2 over the host's one-time password. HKDF-SHA256 takes `X25519 ‖ ML-KEM ‖ SPAKE2` as input keying
  material, so the session stays secret as long as either X25519 or ML-KEM holds (harvest-now-decrypt-later
  resistant). The transcript hash covers ids, both SPAKE messages, both X25519 keys, the ML-KEM key and ciphertext and
  the host's QUIC certificate fingerprint. HKDF derives per-direction AES-256-GCM keys and HMAC confirmation keys. A
  malicious signaling server cannot MITM the session without knowing the password. Protocol version 2 is required on
  both ends.
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

Useful agent flags: `--force-relay`, `--capture synthetic`, `--codec auto|h264|h265|av1`, `--max-bitrate-kbps`,
`--max-fps`, `--log-format json`.

Cargo features of `remoti-core`: `h264` (OpenH264, default), `av1` (rav1e, default) and `av1-asm` (rav1e SIMD,
needs NASM; used for release builds).

## Tests

```sh
cargo test --workspace     # unit tests + in-process e2e (QUIC, relay, wrong password)
cd apps/desktop && npm test && npm run typecheck
```

## Design choices

* **ring** for X25519, AES-256-GCM, HKDF, HMAC and Ed25519, `ml-kem` for ML-KEM-768 and `spake2` for the PAKE.
  The post-quantum share is hybrid rather than a replacement, so a flaw in a young ML-KEM implementation cannot weaken
  the classical guarantee.
* **WebSocket signaling** instead of libp2p. It is simpler to operate behind standard load balancers, and the same
  service hosts the relay.
* **Capture** is cfg-gated: DXGI Desktop Duplication on Windows, ScreenCaptureKit on macOS 12.3+ (idle frames are
  filtered by the frame status, falls back to CoreGraphics `CGDisplay` capture if the stream cannot start). DXGI dirty
  rects feed the delta tracker, which also hashes 64×64 tiles so unchanged frames are skipped.
* **Codecs**: the viewer probes `VideoDecoder.isConfigSupported` and advertises what it can decode; the host picks the
  first match from `[--codec, H.265, H.264, AV1]` that it can encode, or rejects with `NoCommonCodec`.

  | Codec | Windows | macOS | Software fallback |
  |-------|---------|-------|-------------------|
  | H.264 | Media Foundation hardware MFT (NV12, CBR, no B-frames) | VideoToolbox (hardware only, low-latency RC) | OpenH264 |
  | H.265 | Media Foundation hardware MFT | VideoToolbox | none |
  | AV1   | — | — | rav1e (speed 10, low latency) |

  All H.26x output is Annex B with parameter sets on keyframes; AV1 output is one OBU temporal unit per frame with the
  sequence header on keyframes, so WebCodecs needs no `description`. If a hardware encoder fails mid-session the
  pipeline switches to the software encoder for the same codec and forces a keyframe. rav1e is the slowest option,
  which is why AV1 is last in the automatic order.
* **Congestion**: a delay- and loss-based controller fed by QUIC stats and viewer reports, with multiplicative decrease,
  additive increase and application-limited detection. It throttles FPS in steps from 10 to 60.

## Releases

Pushing a `v*` tag runs `.github/workflows/release.yml`. It builds the agent with `--features av1-asm`, packages a
Windows NSIS installer and a macOS (arm64, 12.3+) DMG and ZIP with electron-builder, builds the Linux signaling
server tarball, and attaches everything plus `SHA256SUMS.txt` to a draft GitHub release.

Signing is optional and comes from repository secrets: `WIN_CSC_LINK` / `WIN_CSC_KEY_PASSWORD` for Windows,
`MAC_CSC_LINK` / `MAC_CSC_KEY_PASSWORD` for macOS, and `APPLE_ID` / `APPLE_APP_SPECIFIC_PASSWORD` / `APPLE_TEAM_ID`
for notarization. Without them the installers are built unsigned and the run logs a warning.

```sh
cd apps/desktop && npm run package   # local installer for the current OS into apps/desktop/release/
```
