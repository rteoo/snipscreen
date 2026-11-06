# AGENTS.md — Snipscreen

Snipscreen is a lightweight Windows screenshot and screen-recording tool in Rust, modelled on the
Windows Snipping Tool. Windows-only: it talks to Win32, DXGI, WASAPI and Media Foundation directly.

## Layout

| Path | What it is |
|---|---|
| `app/` | The Cargo crate (`snipscreen`, single binary). All source lives here. |
| `app/src/main.rs` | egui app: state machine (`Stage`: Idle → Waiting → Picking / Recording), toolbar, canvas, picker, recording bar, CLI. |
| `app/src/win.rs` | Thin Win32 layer: screen grab (GDI), window list, clipboard, save dialog, global hotkey, monitor rects, capture exclusion. |
| `app/src/markup.rs` | Snip document: image, pen/highlighter strokes, eraser, crop, freeform cut, 50-step undo. Pure logic, unit-tested. |
| `app/src/recorder.rs` | Screen recording to MP4 (H.264 + AAC) via DXGI duplication + Media Foundation sink writer, 30 fps, max side 4096. |
| `app/src/audio.rs` | WASAPI capture: system loopback and microphone, 48 kHz stereo f32, plus mixing. |
| `app/src/icons.rs` | Toolbar icons drawn as vector strokes (no image assets). |
| `app/assets/` | App icon: `snipscreen-icon.svg` (full) and `snipscreen-icon-small.svg` (16–24 px) are the sources, matching the SnipVoice/SnipType tile and palette; the `.png` renders and `snipscreen.ico` are generated from them. `main.rs` embeds `snipscreen-icon-256.png` as the window/taskbar icon; `app/build.rs` embeds `snipscreen.ico` in the `.exe` through `winresource`. |
| `app/shots/` | Hand-taken UI screenshots and a self-test log from development. Reference only. |
| `craft.json`, `feed.json`, `usage.json`, `checks/`, `capture/`, `investigation.cany/`, `pinned/` | State from the builder tool that generated v1 (request, check rounds, open design questions, cost log). Not app code. Don't edit by hand. |

Open design question carried from the builder (`investigation.cany/unknowns/unk_d23fe80afb17c3ef.json`):
whether to add shapes/arrows, text actions and share. Ask before building those.

## Commands

Run from `app/`.

| Task | Command |
|---|---|
| Build (dev, opt-level 2) | `cargo build` |
| Release build | `cargo build --release` → `target/release/snipscreen.exe` |
| Run | `cargo run` |
| Tests | `cargo test` |
| Device-dependent tests (sound hardware) | `cargo test -- --ignored` |
| Lint | `cargo clippy --all-targets` |
| Headless UI screenshot | `cargo run -- --screenshot out.png` (renders 120 frames, writes PNG, exits; skips the global hotkey and launch-into-picking) |

Toolchain verified: Rust 1.99.1 stable, MSVC target. No `rust-toolchain` file. The build also needs the Windows SDK's `rc.exe` (installed with the MSVC build tools) to embed the `.exe` icon.

## Conventions

- **Formatting is not rustfmt-managed.** The code uses long lines; `cargo fmt --check` reports ~124 diffs. Never run `cargo fmt` across the tree as part of another change. Match surrounding style.
- `cargo clippy --all-targets` is clean. Keep it that way.
- egui's `consume_shortcut` ignores extra Shift/Alt, so consume the more specific shortcut first (Ctrl+Shift+Z before Ctrl+Z). Key handling lives in `read_keys` in `main.rs` and is unit-tested.
- `unsafe` lives only in `win.rs`, `recorder.rs`, `audio.rs`. Keep it there; `main.rs`, `markup.rs`, `icons.rs` stay safe.
- Errors cross module boundaries as `Result<_, String>` and surface as a toast via `App::say`. No panics on user paths.
- Slow work (grab, clipboard, PNG save, recording) runs on a spawned thread and reports back over `mpsc` channels plus `ctx.request_repaint()`. Keep the UI thread non-blocking.
- Deliberate limits are marked `// ceiling:` with the upgrade trigger (see the multi-monitor picker in `main.rs`).
- Colours sampled from the original are constants at the top of `main.rs`. Change them there, not inline.
- Output folders: snips auto-save to `Pictures\Screenshots\Snip <timestamp>.png`; recordings go to `Videos\Screen Recordings\Screen Recording <timestamp>.mp4`.
- Global hotkey is Alt+Shift+S (Win+Shift+S and Print Screen belong to the system tool).

## Testing

- `markup.rs`, `recorder.rs` and the shortcut mapping in `main.rs` have unit tests that need no screen or devices. Shortcut tests drive a headless `egui::Context`; clear `textures_delta` on its output or epaint's debug assert fires. `recorder::tests::encodes_synthetic_mp4` encodes and decodes a real MP4 through Media Foundation and intentionally leaves `target/test-synthetic.mp4` for inspection.
- `audio.rs` tests are `#[ignore]` because they need sound devices.
- Beyond shortcuts, UI behaviour has no automated tests. Verify UI changes by running the app, or with `--screenshot` for a static render.

## Repo state

- `.gitignore` excludes `app/target/` (several GB of build output). Never stage build output.
