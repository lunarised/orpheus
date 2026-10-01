# Orpheus

Orpheus is a lightweight music display and multi-client web controller for a
Mopidy-based player. It targets a Raspberry Pi with an NV3007 SPI display, but
also provides a desktop preview mode for development.

The application includes playback controls, queue and playlist management,
library browsing, playback history, album-art and clock screensavers, weather
and morning information, Spotify Connect status, and a live web interface.

## Build and test

The default feature builds the desktop preview:

```sh
cargo build --locked
cargo test --locked
```

Build the Raspberry Pi hardware version with:

```sh
cargo build --locked --release --no-default-features --features hardware
```

Formatting and lint checks use the standard Rust tools:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
```

## Local configuration

Runtime configuration is deliberately not tracked. On first run Orpheus
creates a local `settings.toml`, including a randomly generated settings
password. Playlist definitions live in `playlists.toml`; queue and history
state are also written locally. All of these paths are ignored by Git.

The display currently expects a locally installed
`fonts/NotoSansCJK-Regular.ttc`. Font binaries and other machine-specific
assets are intentionally excluded from this source repository.

Useful environment variables include:

- `MOPIDY_HOST` and `MOPIDY_PORT` for the Mopidy JSON-RPC endpoint.
- `ORPHEUS_WEB_BIND` for the web interface bind address.
- `ORPHEUS_VISUALIZER_BIND` for visualizer input.
- `ORPHEUS_ASSETS` for local runtime assets.

Do not commit populated runtime configuration files: they may contain service
credentials, an administrator password, location data, or private library
details.
