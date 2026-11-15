# AGENTS.md

Instructions for AI coding agents working on LanCast. Humans: see [README.md](README.md).

LanCast casts a Windows PC's screen and audio to a smart TV (or any browser) over the LAN.
The TV needs no app: it plays a page served by LanCast in its built-in browser. Rust 2024,
one binary, Windows only for now (Linux/macOS are on the roadmap).

If `AGENTS.local.md` exists, read it too: it holds the maintainer's machine-specific notes
(test hardware, TV address). It is gitignored; never copy its contents into tracked files.

## Commands

Run these before calling a change done. CI (`.github/workflows/ci.yml`) runs the same on Windows.

```sh
cargo fmt --check                                     # formatting (rustfmt.toml: 150 cols)
cargo clippy --all-targets --locked -- -D warnings    # lint, warnings are errors
cargo test --locked                                   # unit tests, no hardware needed
```

Hardware tests (need a display and a GPU encoder; run when touching capture, encoding or the pipeline):

```sh
cargo test --release -- --ignored --skip tv_ --test-threads=1   # capture, NVENC/AMF/QSV, H.264/HEVC/AV1, MSE, restarts
                                                      # (one at a time: parallel pipelines share the GPU and miss 4K60)
LANCAST_TV=<ip> cargo test --release tv_ -- --ignored --nocapture   # casts to a real LG webOS TV
```

Run the app: `cargo run` (GUI) or `cargo run -- --headless [--cast <tv-ip>] [--seconds N]`
(prints live stats every second). `cargo run -- --discover` lists TVs. The player is served on
port 7878; `RUST_LOG=lancast=debug` also logs the player's telemetry.

## Layout

The module table in README's "Code map" is the reference; keep it current. Things that aren't obvious:

- `src/web/player.html` and `clock.html` are embedded with `include_str!`. Editing them needs a
  rebuild; there is no separate web build or JS tooling.
- `src/platform/` is the only OS-specific code. `platform/mod.rs` defines the interface
  (`monitors`, `encoders`, `run_video`, `run_audio`) and a non-Windows stub that must keep compiling.
- `src/smoke.rs` holds the `#[ignore]`d hardware tests; unit tests live next to the code.
- `docs/` is the GitHub Pages website (one static `index.html`, no build step).
- `src/webos_manifest.json` is LG's published Connect SDK manifest. Don't edit it.

## Code style

- Small, plain code. Few dependencies, added with `default-features = false`. HTTP, WebSocket
  and fMP4 are hand-written on purpose; ask before adding a crate.
- Formatting is rustfmt with `rustfmt.toml`. For hand-aligned tables (byte layouts, matrices),
  use `#[rustfmt::skip]` with a short reason instead of letting them explode.
- Comments explain *why*: the hardware or browser quirk behind a number or workaround.
  Don't narrate what the code does.
- Errors: `anyhow`, with `.context(...)` where the cause isn't obvious. Outside tests, no
  `unwrap()` on I/O or anything a peer sends; a bad request must not take the app down.
- Match the existing tone in user-facing text (UI, README, website): short, plain language.

## Testing rules

- Tests must never touch the real settings or certificate in `%APPDATA%\lancast\`:
  `config::PERSIST` is false under `cfg(test)`. Keep it that way for new persisted state.
- Server tests treat every peer as remote (`!cfg!(test)` in `server::handle`), so access-policy
  tests behave the same on any machine.
- New logic (parsers, muxers, policies, rate control) gets unit tests. Bugs found on hardware
  get a regression test where they can be reproduced without it.

## Verifying on real devices

Stats and logs are not proof that a TV shows a picture.

- On LG webOS, WebRTC connects and decodes with fine stats while the screen stays **black**
  (frames go to a hidden hardware layer; canvas copies are black too). Only the MSE player
  displays there. After any player change, ask the human what the TV actually shows.
- Testing players on the PC: headless Chrome against `http://127.0.0.1:7878`. Localhost counts
  as a secure context, so WebCodecs works there; a TV on plain HTTP doesn't get WebCodecs.
  Chrome's `--screenshot` fires too early; drive it over CDP instead (Node has a built-in WebSocket).
- Latency claims come from measurements (player telemetry, the latency-check clock page), never
  estimates. Update README's "Measured" table when they change.

## Known traps

- `platform/windows::com_init` calls `CoIncrementMTAUsage` once. Without it, GraphicsCapture.dll
  unloads when the last MTA thread exits and the process crashes (0xC0000005) when a pipeline stops.
- The MSE player's buffer target, 1.1x catch-up rate and speed-change hysteresis were tuned on a
  real TV; lower values stall it. The encoder's two-frame VBV stops bitrate bursts that starve the
  TV's buffer. Don't change these without a TV test.
- Settings JSON must be written without a BOM. PowerShell's `Set-Content -Encoding utf8` adds one.
- A disconnected network adapter can steal multicast, which is why SSDP binds to the LAN IP.
- The TV browser keeps background tabs alive, and stale tabs block new WebRTC sessions. The player
  releases everything when hidden; keep that.

## Boundaries

**Always:** run fmt, clippy and tests; keep README (CLI usage, code map, tables) in sync with behavior.

**Ask first:** adding dependencies; changing the wire format in `stream.rs` (the player must change
with it); changing defaults or presets; editing CI or the release workflow; committing or pushing.

**Never:**
- Push tags (`v*` publishes a release) or force-push.
- Put personal details in tracked files, commits or the website: no specific TV or PC hardware models,
  no real IPs (use `192.168.1.50` in examples), no names or emails. Say "an LG webOS TV".
- Commit secrets, generated certificates or anything from `%APPDATA%\lancast\`.

## Commits

Short imperative subject ("Add HEVC to the MSE player"), body only when the why isn't obvious.
One logical change per commit; formatting-only changes go in their own commit.
