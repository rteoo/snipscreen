# Snipscreen

A small, fast screenshot and screen-recording tool for Windows, written in Rust with egui.
It follows the Windows Snipping Tool's workflow: launch, pick an area, mark it up, copy or save.

## Features

**Screenshots**
- Rectangle, window, full-screen and freeform snips
- Delay timer: none, 3, 5 or 10 seconds
- Markup: pen (6 colours), highlighter (4 colours), eraser, crop, undo/redo
- Auto-copies to the clipboard and auto-saves to `Pictures\Screenshots` (both can be turned off)

**Screen recording**
- Rectangle, window or full-screen area, recorded to MP4 (H.264 + AAC, 30 fps)
- System audio and microphone, each mutable, with a microphone picker
- A dashed outline marks the area being recorded and stays out of the video
- Saved to `Videos\Screen Recordings`

## Requirements

- Windows 10 version 2004 or later (Windows 11 recommended)
- Rust stable with the MSVC toolchain to build

## Build and run

```bash
cd app
cargo build --release
```

The binary is `app/target/release/snipscreen.exe`. For a dev run:

```bash
cargo run
```

The app opens straight into area picking. Close the picker to get the main window.

## Shortcuts

| Keys | Action |
|---|---|
| Alt+Shift+S | New snip, from anywhere in Windows |
| Ctrl+N | New snip |
| Ctrl+C | Copy the snip with markup |
| Ctrl+S | Save as PNG (or save the recording as MP4) |
| Ctrl+Z | Undo |
| Ctrl+Y or Ctrl+Shift+Z | Redo |
| Enter | Apply crop |
| Esc | Cancel crop, picking, or close the recording bar |

## Tests

```bash
cd app
cargo test
```

Tests that need sound hardware are ignored by default; run them with `cargo test -- --ignored`.

## Project layout

```
app/
  src/main.rs      UI, state machine, CLI
  src/win.rs       Win32: screen grab, clipboard, dialogs, hotkey
  src/markup.rs    Snip document: strokes, crop, undo
  src/recorder.rs  MP4 recording via Media Foundation
  src/audio.rs     WASAPI loopback and microphone capture
  src/icons.rs     Vector toolbar icons
```

See `AGENTS.md` for contributor and agent conventions.
