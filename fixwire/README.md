<div align="center">

_Bugs reach production. Fixwire finds them first: errors, traces, logs and AI agent runs in one place, an AI debugger on every plan, and your data kept in Europe._

[![Discord](https://img.shields.io/badge/Discord-join%20us-5865F2?logo=discord&logoColor=white)](https://fixwire.io/discord)
[![Slack](https://img.shields.io/badge/Slack-community-4A154B?logo=slack&logoColor=white)](https://fixwire.io/slack)
[![X](https://img.shields.io/badge/X-follow%20us-000000?logo=x&logoColor=white)](https://fixwire.io/x)
[![Release](https://img.shields.io/github/v/release/fixwire/fixwire-rust?label=release)](https://github.com/fixwire/fixwire-rust/releases)
[![Rust](https://img.shields.io/badge/rust-1.88%2B%20%7C%20stable-blue?logo=rust)](https://github.com/fixwire/fixwire-rust/blob/main/.github/workflows/ci.yml)
[![CI](https://github.com/fixwire/fixwire-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/fixwire/fixwire-rust/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/fixwire/fixwire-rust/blob/main/LICENSE)

<br/>

</div>

# Fixwire SDK for Rust

Welcome to the official Rust SDK for **[Fixwire](https://fixwire.io)**. It
reports errors and panics, traces, release health, cron monitor check-ins
and user feedback from your Rust services and jobs.

This is the crate's short guide; the
[repository README](https://github.com/fixwire/fixwire-rust#readme) has
the full one.

## 📦 Getting started

### Prerequisites

- A Fixwire account and a project: sign up at [fixwire.io](https://fixwire.io).
- Rust 1.88 or newer.

### Installation

```toml
[dependencies]
fixwire = "0.1"
# a server on axum, hyper or tonic, and tracing:
# fixwire = { version = "0.1", features = ["axum", "tracing"] }
```

### Basic configuration

Call `fixwire::init` first thing in `main` and keep the guard it returns
until the program ends: when it is dropped, it sends what is left.

```rust,no_run
fn main() {
    let _fixwire = fixwire::init(fixwire::Options {
        dsn: Some("https://fw_pk_live_…@ingest.eu.fixwire.io".into()),
        release: Some("api@1.4.0".into()),
        environment: Some("production".into()),
        traces_sample_rate: 0.2,
        // send_default_pii: true, // also send IP addresses, cookies and Authorization headers
        // redact: false,          // turn off on-device masking of secrets and personal data
        ..Default::default()
    });

    // your program
}
```

The DSN is your project's publishable key and the ingest host:
`https://<publishable key>@<host>`. Without the `dsn` option the SDK reads
`FIXWIRE_DSN`; without either it does nothing. An invalid DSN doesn't stop
your app: `init` never panics, it says so on stderr and leaves the SDK
off. `FIXWIRE_RELEASE` and `FIXWIRE_ENVIRONMENT` work the same way.

### Quick usage example

```rust,no_run
fn main() {
    let _fixwire = fixwire::init(fixwire::Options::default()); // the DSN from FIXWIRE_DSN

    // Shows up as an info message, with the scope's tags and breadcrumbs.
    fixwire::capture_message("Hello Fixwire!", fixwire::Level::Info);

    // Shows up as a ParseIntError issue, with the stack where it was captured.
    if let Err(e) = "4x".parse::<u32>() {
        fixwire::capture_error(&e);
    }
}
```

Panics are reported as crashes, and the panic hook that was there still
runs. Scopes, traces, cron monitors and feedback:

```rust,no_run
fixwire::configure_scope(|s| {
    s.set_user(Some(fixwire::User::with_id("user-1")));
    s.set_tag("plan", "team");
});

let total = fixwire::trace("SELECT orders", "db.query", |span| {
    span.set_attribute("db.system.name", "postgresql");
    42
});

let report = fixwire::with_monitor(
    "nightly-report",
    Some(fixwire::MonitorConfig::crontab("0 3 * * *")),
    || -> Result<(), std::io::Error> { Ok(()) },
);

fixwire::capture_feedback(fixwire::Feedback {
    message: Some("The refund amount was wrong".into()),
    score: Some(-1.0),
    ..Default::default()
});
```

## ✨ Why Fixwire

- **Secrets stay on the device**, masked with the same rules as the Fixwire
  server before anything is sent.
- **A crash loop costs a few events and a count**, not your quota.
- **It never gets in your app's way**: `init` never panics, captures never
  block, and memory, payload sizes and shutdown time have strict limits.
- **OpenTelemetry-native**: the Fixwire protocol is OpenTelemetry's
  OTLP/HTTP plus a few small JSON endpoints.
- **Trace headers only where you allow**, none by default.
- **Made for Rust**: no `unsafe` code and no async runtime needed.

## 🧩 Integrations

| Integration | What it does | How to use |
| --- | --- | --- |
| tower (`tower` feature) | A layer for axum, hyper and tonic servers: a scope, a server span and a release health session per request | [Servers](https://github.com/fixwire/fixwire-rust#servers-tower-and-axum) |
| axum (`axum` feature) | The tower layer, with each request named after its route | [Servers](https://github.com/fixwire/fixwire-rust#servers-tower-and-axum) |
| tracing (`tracing` feature) | Events become breadcrumbs, error events become Fixwire events | [Logging with tracing](https://github.com/fixwire/fixwire-rust#logging-with-tracing) |
| Any HTTP client | `OutgoingRequest`: a client span and trace headers for your targets | [Traces](https://github.com/fixwire/fixwire-rust#traces) |

```toml
[dependencies]
fixwire = { version = "0.1", features = ["axum", "tracing"] }
```

```rust,ignore
let app = axum::Router::new()
    .route("/orders/{id}", axum::routing::get(order))
    .layer(fixwire::tower::FixwireLayer::new());

use tracing_subscriber::prelude::*;
tracing_subscriber::registry().with(fixwire::tracing::layer()).init();
```

## ⚙️ Configuration

`fixwire::Options` holds every option; the ones most apps set are `dsn`,
`release`, `environment`, `traces_sample_rate`,
`trace_propagation_targets`, `before_send` and `send_default_pii`. The
[configuration guide](https://github.com/fixwire/fixwire-rust#%EF%B8%8F-configuration)
lists them all with their defaults, and explains sampling, trace
propagation targets, `before_send`, redaction, the error budget and the
limits.

## 🧪 Examples

- [shop-api](https://github.com/fixwire/fixwire-rust/blob/main/examples/shop-api/main.rs):
  an axum API with the tower layer and the `tracing` layer.
- [nightly-report](https://github.com/fixwire/fixwire-rust/blob/main/examples/nightly-report/main.rs):
  a cron job reporting to a monitor.

## 📚 Documentation

The full guide lives in the
[repository README](https://github.com/fixwire/fixwire-rust#readme) and the
examples.

- [Configuration](https://github.com/fixwire/fixwire-rust#%EF%B8%8F-configuration)
- [Examples](https://github.com/fixwire/fixwire-rust/tree/main/examples)
- [Changelog](https://github.com/fixwire/fixwire-rust/blob/main/CHANGELOG.md)
- [Security policy](https://github.com/fixwire/fixwire-rust/blob/main/SECURITY.md)
- [Contributing guide](https://github.com/fixwire/fixwire-rust/blob/main/CONTRIBUTING.md)

## 🚧 Coming from another error tracker?

The API follows the shape most error-tracking SDKs share (`init`,
`capture_error`, `capture_message`, `set_user`, `set_tag`,
`add_breadcrumb`, spans), so moving over is mostly a change of crate and
DSN. `init` returns a guard to keep until the program ends.

## 🙌 Want to contribute?

Contributions are welcome, from a typo fix to a new integration. Read the
[contributing guide](https://github.com/fixwire/fixwire-rust/blob/main/CONTRIBUTING.md),
then pick one of the [open issues](https://github.com/fixwire/fixwire-rust/issues)
or a [good first issue](https://github.com/fixwire/fixwire-rust/issues?q=is%3Aopen+label%3A%22good+first+issue%22).

## 🛟 Need help?

- Questions: ask on [Discord](https://fixwire.io/discord) or
  [Slack](https://fixwire.io/slack).
- Bugs: open a [GitHub issue](https://github.com/fixwire/fixwire-rust/issues).
- Found a security issue? Please don't open an issue; follow the
  [security policy](https://github.com/fixwire/fixwire-rust/blob/main/SECURITY.md).

## 🔗 Resources

- [Website](https://fixwire.io)
- [Pricing](https://fixwire.io/pricing)
- [Discord](https://fixwire.io/discord)
- [Slack](https://fixwire.io/slack)
- [X](https://fixwire.io/x)
- [Changelog](https://github.com/fixwire/fixwire-rust/blob/main/CHANGELOG.md)
- [Examples](https://github.com/fixwire/fixwire-rust/tree/main/examples)
- [Security policy](https://github.com/fixwire/fixwire-rust/blob/main/SECURITY.md)

## 📃 License

The SDK is open source under the MIT license; see
[LICENSE](https://github.com/fixwire/fixwire-rust/blob/main/LICENSE).

## 😘 Contributors

Thanks to everyone who helps make Fixwire better!

<a href="https://github.com/fixwire/fixwire-rust/graphs/contributors"><img src="https://contrib.rocks/image?repo=fixwire/fixwire-rust" alt="Contributors" /></a>
