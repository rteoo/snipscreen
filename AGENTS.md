# AGENTS.md — Snipscreen

Snipscreen is a lightweight Windows screenshot and screen-recording tool in Rust, modelled on the
Windows Snipping Tool. Windows-only: it talks to Win32, DXGI, WASAPI and Media Foundation directly.

## Layout

| Path | What it is |
|---|---|
| `app/` | The Cargo crate (`snipscreen`, single binary). All source lives here. |
| `app/src/main.rs` | The shell (`App`): home window (New, photo/video, shape, delay, "…" with Settings… and the Screenshots folder), state machine (`Stage`: Idle → Counting → Waiting → Picking / Recording), picker, recording bar, hotkeys, CLI. The root window is the home window and turns into the picker or recording bar during a capture. Every finished snip goes through `App::on_captured`: with "after a snip: notification" (the default) `notify_snip` copies/saves on a worker (PNG encoded once, preview made alongside) and raises a Windows toast; with "open the editor", `open_editor` plus `auto_copy_save`. |
| `app/src/notify.rs` | Windows toast notifications (WinRT `ToastNotificationManager`) for an unpackaged app: `register` writes the AppUserModelID `SnipScreen.App` (`HKCU\Software\Classes\AppUserModelId\SnipScreen.App`, icon at `%LOCALAPPDATA%\SnipScreen\icon.png`) and the `snipscreen:` link handler (`HKCU\Software\Classes\snipscreen`). A toast's click is protocol activation of `snipscreen:open?id=<id>&path=<png>`, which reaches the running copy through `tray::forward` (or starts one hidden) and opens an editor from `Recent` (in memory while the toast is on screen and the file is written, at most 10 snips / 64 MB / 3 minutes) or the PNG. The toast goes up as soon as the preview is made; the PNG encode, copy and save run alongside, and a toast that turns out wrong is replaced in place (same tag) with one that says what failed. Previews and not-auto-saved snips live in `%LOCALAPPDATA%\SnipScreen\recent`, pruned to the newest 20. Links are untrusted: only PNGs directly in Pictures\Screenshots or `recent` are opened (`may_open`). |
| `app/src/editor.rs` | One result per native window: `Editor` (a `Snip` being marked up, or a `VideoResult`), its toolbar, canvas, shortcuts (`read_keys`), copy/save and toast. `Editors` owns the open windows; each is a deferred egui viewport behind `Arc<Mutex<Editor>>`, so it repaints only for its own input. |
| `app/src/widgets.rs` | Shared look: colour and size constants, egui style, `title_bar` (the 40 px toolbar row with Windows 11 caption buttons), `icon_button`/`dropdown`/`divider`/`new_button`, `dashed_outline` (picker selection and recording area), toasts (`Note`, `Notifier`), `PopupClickGuard`, the Esc rule. |
| `app/src/settings.rs` | Persisted choices in `%APPDATA%\SnipScreen\settings.ini` (`key=value`): shortcut (`Hotkey`), after-capture behaviour, auto copy/save, start with Windows, capture mode/shape/delay, recording sound. Unreadable values fall back to defaults and are reported; `Saver` writes atomically (temp + rename) on its own thread. The shell's `Mode`/`Shape` live here. Pure logic plus file IO, unit-tested. |
| `app/src/tray.rs` | The resident side: one thread with a hidden top-level window (class `SnipScreen.Agent`) that owns the tray icon and its menu (re-added on Explorer's `TaskbarCreated`), the global snip shortcut and the countdown's Esc, and takes `WM_COPYDATA` from later launches. `claim_single_instance` (named mutex `Local\SnipScreen.Instance`) and `forward` make a second launch hand its `Intent` (show / `--tray` / `--quit` / a `snipscreen:` link) to the running copy and exit. Everything reaches the shell as a `tray::Event` over one channel. `sync_run_at_startup` keeps `HKCU...CurrentVersionRunSnipScreen` = `"<exe>" --tray` in step with the setting (checked on every start, so a moved exe is picked up; a dev build run this way points the entry at `target`). A `WM_CLOSE` from outside (`taskkill` without /f) is a Quit request; only `Agent::drop` destroys the window. `WM_ENDSESSION` waits up to 5 s for the shell's exit (`Agent::exited`), and while the shell exits later launches are refused (`Forwarded::Closing`) and wait to start on their own. |
| `app/src/settings_window.rs` | The small Settings window (deferred viewport, opened from the tray menu and the home "…" menu): shortcut with a recorder, after-a-snip behaviour, auto copy/save, start with Windows. Edits a copy and raises flags the shell collects in `poll`; the recorder itself is `tray::Agent::record`, a low-level keyboard hook live only while the window is focused and waiting for a key. |
| `app/src/win.rs` | Thin Win32 layer: screen grab (GDI), window list, clipboard, save dialog, known folders, monitor rects, capture exclusion, and `TitleBar`, the window subclass that turns the home and editor windows' toolbar into their title bar. |
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
| Windows installer (from the repo root, after the release build) | `build_installer.bat` → `installer/Output/SnipscreenSetup-<version>.exe` (Inno Setup 6, per-user; set `SNIPSCREEN_BUILD_NONINTERACTIVE=1` to skip the pauses). Keep `MyAppVersion` in `installer/snipscreen.iss` equal to the `Cargo.toml` version. Install and uninstall run `snipscreen.exe --quit` (only when the instance mutex exists, without waiting on that launch) and wait for the mutex to go; uninstall removes the HKCU Run value, the `SnipScreen.App` AUMID key, the `snipscreen` protocol key, and what Windows keeps (Notifications\Settings\SnipScreen.App, StartupApproved\Run\SnipScreen). |
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
- `unsafe` lives only in `win.rs`, `tray.rs`, `notify.rs`, `recorder.rs`, `audio.rs`. Keep it there; `main.rs`, `editor.rs`, `widgets.rs`, `markup.rs`, `icons.rs` stay safe.
- Errors cross module boundaries as `Result<_, String>` and surface as a toast: `App::say` in the shell, `Editor::say` inside an editor's own frame, and a `widgets::Notifier` from worker threads (it also wakes the right window). From another thread, wake the shell with `request_repaint_of(ViewportId::ROOT)` (`main::wake_root`), never `request_repaint`, which targets whichever window is mid-pass. No panics on user paths.
- Slow work (grab, clipboard, PNG save, recording) runs on a spawned thread and reports back over `mpsc` channels plus `ctx.request_repaint()`. Keep the UI thread non-blocking.
- Editor windows are deferred viewports: `Editors::show` must run on every pass of the root window (egui closes a viewport its parent stops showing), the root never blocks on an editor's lock, and an editor talks back only through its `new_requested`/`closed` flags, collected by `Editors::poll`. Don't switch them to immediate viewports: those re-run the root and every other editor (and their vsync'd swaps) on each repaint.
- The home and editor windows draw their own title bar: `win::TitleBar` subclasses the window (`SetWindowSubclass`), removes the caption in `WM_NCCALCSIZE` but keeps the native frame (DWM shadow, resize borders, snap, animations), and answers `WM_NCHITTEST` from the rects `widgets::title_bar` publishes each frame: HTCAPTION over the empty bar, HTTOP along the top edge, HTMAXBUTTON over our maximise button (Windows 11 snap layouts; its hover/press come from the subclass, its click from the frame). winit still sizes windows as if they had a caption, so send `ViewportCommand::InnerSize` for these windows through `TitleBar::inner_size_for` (or size them with `win::size_client`); min size and DPI changes are corrected inside the subclass. The shell's window hands the frame back to winit (`set_active(false)`) while it is the frameless picker or recording bar. The Settings window keeps the native caption.
- Deliberate limits are marked `// ceiling:` with the upgrade trigger (see the multi-monitor picker in `main.rs`).
- Colours sampled from the original are constants at the top of `widgets.rs`. Change them there, not inline.
- Output folders: snips auto-save to `Pictures\Screenshots\Snip <timestamp>.png`; recordings go to `Videos\Screen Recordings\Screen Recording <timestamp>.mp4`. Recordings always open their result window; only snips go through the notification.
- A resident run sets the process AppUserModelID to `SnipScreen.App` before any window exists (`notify::set_process_id`); the installer's Start-menu shortcut carries the same ID. To try a toast click without clicking, open its link: `Start-Process "snipscreen:open?id=1&path=<pct-encoded png>"`. The handler is `"<exe>" --open-link "%1"`: after `--open-link` nothing is parsed as a flag (`main::parse_args`), and links with quotes or whitespace are refused, since any program can open a `snipscreen:` link.
- The app is resident: closing the home window conceals it, and only the tray menu's Quit (or `--quit`, or signing out) ends the process, after a running recording is saved. The root window is never hidden: out of sight it stays shown but cloaked, off the taskbar and without the foreground (`App::conceal`/`reveal`), because eframe delays a hidden window's repaints (and timers) by 100 ms or more. Never send it `ViewportCommand::Visible`. `--tray` starts cloaked and inactive across eframe's show-after-first-frame (`HiddenStart` in `main.rs`), so nothing flashes or takes focus.
- Only one copy runs. Started again, it forwards to the running one (`main::start_resident`) before doing any other work; screenshot runs (`--screenshot*`) skip this and never take the shortcut. `cargo run` while the app is running just brings the running copy up.
- Global hotkey defaults to Alt+Shift+S (Win+Shift+S and Print Screen belong to the system tool).

## Testing

- `markup.rs`, `recorder.rs`, the shortcut mapping in `editor.rs`, the `Editors` window list and the title-bar hit test in `win.rs` have unit tests that need no screen or devices. They drive a headless `egui::Context`, where egui embeds each viewport in the calling frame, so editor code runs as it does in its own window; clear `textures_delta` on its output or epaint's debug assert fires. `recorder::tests::encodes_synthetic_mp4` encodes and decodes a real MP4 through Media Foundation and intentionally leaves `target/test-synthetic.mp4` for inspection.
- `audio.rs` tests are `#[ignore]` because they need sound devices.
- Beyond shortcuts, UI behaviour has no automated tests. Verify UI changes by running the app, or with `--screenshot` / `--screenshot-editor` for a static render.

## Repo state

- `.gitignore` excludes `app/target/` (several GB of build output). Never stage build output.
