# Az Silent Keep Awake
A lightweight Windows 11 system tray app written in Rust.
- Plays silent audio in the background.
- Prevents PC from sleeping during custom schedules.
- Portable single `.exe` with embedded audio and icon.

## How to Build
1. Ensure `icon.ico` and `silent.wav` are in the root folder.
2. Run `cargo build --release`.
3. The `.exe` will be in `target/release/`.