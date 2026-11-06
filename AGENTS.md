# AGENTS.md — Snipscreen

Snipscreen is a lightweight Windows screenshot and screen-recording tool in Rust, modelled on the
Windows Snipping Tool. Windows-only: it talks to Win32, DXGI, WASAPI and Media Foundation directly.

## Layout

| Path | What it is |
|---|---|
| `app/` | The Cargo crate (`snipscreen`, single binary). All source lives here. |
| `app/src/main.rs` | The shell (`App`): home window (New, photo/video, shape, delay, "…" with Settings… and the Screenshots folder), state machine (`Stage`: Idle → Counting → Waiting → Picking / Recording), picker, recording bar, hotkeys, CLI. The root window is the home window and turns into the picker or recording bar during a capture. Every finished snip goes through `App::on_captured` (auto copy/save, then `App::open_editor`). |
| `app/src/editor.rs` | One result per native window: `Editor` (a `Snip` being marked up, or a `VideoResult`), its toolbar, canvas, shortcuts (`read_keys`), copy/save and toast. `Editors` owns the open windows; each is a deferred egui viewport behind `Arc<Mutex<Editor>>`, so it repaints only for its own input. |
| `app/src/widgets.rs` | Shared look: colour and size constants, egui style, `icon_button`/`dropdown`/`divider`/`new_button`, toasts (`Note`, `Notifier`), `PopupClickGuard`, the Esc rule. |
| `app/src/settings.rs` | Persisted choices in `%APPDATA%\SnipScreen\settings.ini` (`key=value`): shortcut (`Hotkey`), after-capture behaviour, auto copy/save, start with Windows, capture mode/shape/delay, recording sound. Unreadable values fall back to defaults and are reported; `Saver` writes atomically (temp + rename) on its own thread. The shell's `Mode`/`Shape` live here. Pure logic plus file IO, unit-tested. |
| `app/src/tray.rs` | The resident side: one thread with a hidden top-level window (class `SnipScreen.Agent`) that owns the tray icon and its menu (re-added on Explorer's `TaskbarCreated`), the global snip shortcut and the countdown's Esc, and takes `WM_COPYDATA` from later launches. `claim_single_instance` (named mutex `Local\SnipScreen.Instance`) and `forward` make a second launch hand its `Intent` (show / `--tray` / `--quit` / a `snipscreen:` link) to the running copy and exit. Everything reaches the shell as a `tray::Event` over one channel. `sync_run_at_startup` keeps `HKCU...CurrentVersionRunSnipScreen` = `"<exe>" --tray` in step with the setting (checked on every start, so a moved exe is picked up; a dev build run this way points the entry at `target`). |
| `app/src/settings_window.rs` | The small Settings window (deferred viewport, opened from the tray menu and the home "…" menu): shortcut with a recorder, after-a-snip behaviour, auto copy/save, start with Windows. Edits a copy and raises flags the shell collects in `poll`; the recorder itself is `tray::Agent::record`, a low-level keyboard hook live only while the window is focused and waiting for a key. |
| `app/src/win.rs` | Thin Win32 layer: screen grab (GDI), window list, clipboard, save dialog, known folders, monitor rects, capture exclusion. |
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
| Windows installer (from the repo root, after the release build) | `build_installer.bat` → `installer/Output/SnipscreenSetup-<version>.exe` (Inno Setup 6, per-user; set `SNIPSCREEN_BUILD_NONINTERACTIVE=1` to skip the pauses). Keep `MyAppVersion` in `installer/snipscreen.iss` equal to the `Cargo.toml` version. |
| Run | `cargo run` (opens the home window; the app then lives in the tray until Quit) |
| Run as at sign-in | `cargo run -- --tray` (starts hidden in the tray; `--quit` ends a running copy) |
| Tests | `cargo test` |
| Device-dependent tests (sound hardware) | `cargo test -- --ignored` |
| Lint | `cargo clippy --all-targets` |
| Headless UI screenshot | `cargo run -- --screenshot out.png` (renders the home window for 120 frames, writes PNG, exits; skips the global hotkey) |
| Headless settings screenshot | `cargo run -- --screenshot-settings out.png` (the Settings window with default settings; no shortcut is registered in a screenshot run, so it reads "None") |
| Headless editor screenshot | `cargo run -- --screenshot-editor out.png` (opens one editor window on a built-in sample snip with a pen and a highlighter stroke, writes that window's PNG, exits) |

Toolchain verified: Rust 1.99.1 stable, MSVC target. No `rust-toolchain` file. The build also needs the Windows SDK's `rc.exe` (installed with the MSVC build tools) to embed the `.exe` icon.

## Conventions

- **Formatting is not rustfmt-managed.** The code uses long lines; `cargo fmt --check` reports ~124 diffs. Never run `cargo fmt` across the tree as part of another change. Match surrounding style.
- `cargo clippy --all-targets` is clean. Keep it that way.
- egui's `consume_shortcut` ignores extra Shift/Alt, so consume the more specific shortcut first (Ctrl+Shift+Z before Ctrl+Z). Editor key handling lives in `read_keys` in `editor.rs` and is unit-tested; the home window only takes Ctrl+N.
- `unsafe` lives only in `win.rs`, `tray.rs`, `recorder.rs`, `audio.rs`. Keep it there; `main.rs`, `editor.rs`, `widgets.rs`, `markup.rs`, `icons.rs` stay safe.
- Errors cross module boundaries as `Result<_, String>` and surface as a toast: `App::say` in the shell, `Editor::say` inside an editor's own frame, and a `widgets::Notifier` from worker threads (it also wakes the right window). No panics on user paths.
- Slow work (grab, clipboard, PNG save, recording) runs on a spawned thread and reports back over `mpsc` channels plus `ctx.request_repaint()`. Keep the UI thread non-blocking.
- Editor windows are deferred viewports: `Editors::show` must run on every pass of the root window (egui closes a viewport its parent stops showing), the root never blocks on an editor's lock, and an editor talks back only through its `new_requested`/`closed` flags, collected by `Editors::poll`. Don't switch them to immediate viewports: those re-run the root and every other editor (and their vsync'd swaps) on each repaint.
- Deliberate limits are marked `// ceiling:` with the upgrade trigger (see the multi-monitor picker in `main.rs`).
- Colours sampled from the original are constants at the top of `widgets.rs`. Change them there, not inline.
- Output folders: snips auto-save to `Pictures\Screenshots\Snip <timestamp>.png`; recordings go to `Videos\Screen Recordings\Screen Recording <timestamp>.mp4`.
- The app is resident: closing the home window hides it, and only the tray menu's Quit (or `--quit`, or signing out) ends the process, after a running recording is saved. `--tray` starts with the root window cloaked and inactive across eframe's show-after-first-frame (`HiddenStart` in `main.rs`), so nothing flashes or takes focus.
- Only one copy runs. Started again, it forwards to the running one (`main::start_resident`) before doing any other work; screenshot runs (`--screenshot*`) skip this and never take the shortcut. `cargo run` while the app is running just brings the running copy up.
- Global hotkey defaults to Alt+Shift+S (Win+Shift+S and Print Screen belong to the system tool).

## Testing

- `markup.rs`, `recorder.rs`, the shortcut mapping in `editor.rs` and the `Editors` window list have unit tests that need no screen or devices. They drive a headless `egui::Context`, where egui embeds each viewport in the calling frame, so editor code runs as it does in its own window; clear `textures_delta` on its output or epaint's debug assert fires. `recorder::tests::encodes_synthetic_mp4` encodes and decodes a real MP4 through Media Foundation and intentionally leaves `target/test-synthetic.mp4` for inspection.
- `audio.rs` tests are `#[ignore]` because they need sound devices.
- Beyond shortcuts, UI behaviour has no automated tests. Verify UI changes by running the app, or with `--screenshot` / `--screenshot-editor` for a static render.

## Repo state

- `.gitignore` excludes `app/target/` (several GB of build output). Never stage build output.
