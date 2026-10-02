# Contributing

Keep changes focused and submit them through a pull request. Before opening a
review, run:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets
cargo test --locked --all-targets
cargo check --locked --release --no-default-features --features hardware --bins
```

Runtime configuration and state must remain local. Never commit populated
`settings.toml`, `playlists.toml`, queue/history state, credentials, private
network addresses, personal paths, or precise location data.

New behavior should include focused tests. Keep hardware I/O behind the
`hardware` feature so the default desktop build remains useful in CI and local
development.

