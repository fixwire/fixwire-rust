# Fixwire for Rust

[![CI](https://github.com/fixwire/fixwire-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/fixwire/fixwire-rust/actions/workflows/ci.yml)

The Fixwire SDK for Rust: errors and panics, traces, release health, cron
monitors and feedback, with a tower layer for axum, hyper and tonic servers
and a `tracing` layer. Secrets and personal data are masked on the device,
with the same rules as the Fixwire server.

```toml
[dependencies]
fixwire = { version = "0.1", features = ["axum", "tracing"] }
```

The crate's README, [fixwire/README.md](fixwire/README.md), has the details.
[examples](examples) has real programs (an axum API and a cron job), run
against a fake ingest by their tests so they keep working.

## Development

Rust 1.88 or newer:

```sh
cargo test --workspace --all-features
cargo clippy --workspace --all-features --all-targets -- -D warnings
cargo fmt --all --check
```

## License

[MIT](LICENSE)
