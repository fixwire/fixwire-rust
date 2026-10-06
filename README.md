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

## 📦 Getting started

### Prerequisites

- A Fixwire account and a project: sign up at [fixwire.io](https://fixwire.io).
- Rust 1.88 or newer. CI tests Rust 1.88 and the latest stable on Linux,
  macOS and Windows.

### Installation

```sh
cargo add fixwire
```

Or in `Cargo.toml`, with the features for your stack (see
[Integrations](https://github.com/fixwire/fixwire-rust#-integrations)):

```toml
[dependencies]
fixwire = "0.1"
# a server on axum, hyper or tonic, and tracing:
# fixwire = { version = "0.1", features = ["axum", "tracing"] }
```

### Basic configuration

Call `fixwire::init` first thing in `main` and keep the guard it returns
until the program ends. When the guard is dropped, it sends what is left
(waiting up to two seconds) and stops the SDK.

```rust
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
`FIXWIRE_DSN`; without either it does nothing, so the same code runs in
tests and on laptops. An invalid DSN doesn't stop your app: `init` never
panics, it says so on stderr and leaves the SDK off. `FIXWIRE_RELEASE` and
`FIXWIRE_ENVIRONMENT` stand in for `release` and `environment` the same
way.

### Quick usage example

```rust
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

### Errors and panics

`capture_error` sends an error with the stack of the line that captured it
and its chain of `source()`s. The type is the one you captured
(`ParseIntError`), or read from `Debug` for a `dyn Error`. Panics are
reported as crashes, from the line that panicked, and the panic hook that
was there before still runs after Fixwire's (`capture_panics: false` turns
this off). Frames of your crate are the app's; those of the standard
library and your dependencies are not.

Release builds carry no debug information by default, so their stacks
name functions only. With limited debug information they also name
modules, files and lines, inlined calls included, at no cost in speed:

```toml
# Cargo.toml
[profile.release]
debug = "limited"
```

### Scopes, users and breadcrumbs

What the scope knows is added to every event captured with it:

```rust
fixwire::configure_scope(|s| {
    s.set_user(Some(fixwire::User::with_id("user-1")));
    s.set_tag("plan", "team");
});
fixwire::add_breadcrumb(fixwire::Breadcrumb::new("cart", "added 2 items"));

// A scope for one piece of work only.
fixwire::with_scope(
    |s| s.set_tag("job", "import"),
    || fixwire::capture_message("the nightly import found no rows", fixwire::Level::Warning),
);
```

Scopes follow the work. A thread has the process's hub unless `Hub::run`
gives it another, and a future polled with `bind_hub` has its own hub
wherever it is polled (the `tower` layer does this for each request):

```rust
use fixwire::FutureExt;

async fn import() {}

#[tokio::main]
async fn main() {
    let hub = fixwire::Hub::current().fork();
    hub.configure_scope(|s| s.set_tag("job", "import"));
    tokio::spawn(import().bind_hub(hub));
}
```

### Traces

With `traces_sample_rate` above 0, spans are sent as OTLP. A trace
continued from a caller follows the caller's decision; a new one is kept
or dropped the way every Fixwire SDK decides, so your services agree.

```rust
let total = fixwire::trace("SELECT orders", "db.query", |span| {
    span.set_attribute("db.system.name", "postgresql");
    42
});
```

`trace_future` does the same for a future, and `start_span` starts a span
you finish yourself. Outgoing requests, from any HTTP client:

```rust
let call = fixwire::OutgoingRequest::start("POST", "http://inventory.internal/reservations");
// add call.headers() to the request: trace headers, only for trace_propagation_targets
call.finish(Some(201), None);
```

### Cron monitors

A scheduled job reports its runs: in progress, then ok, or error when the
job returns an error or panics. A config creates or updates the monitor.

```rust
let result = fixwire::with_monitor(
    "nightly-report",
    Some(fixwire::MonitorConfig::crontab("0 3 * * *")),
    || -> Result<(), std::io::Error> { Ok(()) },
);
```

### Feedback

`capture_feedback` sends what someone said about an error or an AI answer:
a message, a score from -1 (bad) to 1 (good), or both. A negative score
tied to a trace opens a `user_feedback` issue.

```rust
fixwire::capture_feedback(fixwire::Feedback {
    message: Some("The refund amount was wrong".into()),
    score: Some(-1.0),
    ..Default::default()
});
```

## ✨ Why Fixwire

- **Secrets stay on the device.** Secrets and personal data are masked
  before anything is sent, with the same rules as the Fixwire server. Long
  strings are cut after masking, so a secret the cut goes through is still
  masked.
- **A crash loop costs a few events and a count, not your quota.** The
  error budget sends a burst per issue, then a few a minute, and counts
  the rest on the issue's next event.
- **It never gets in your app's way.** `init` never panics. Captures never
  block: one thread sends from a bounded queue, retries with backoff and
  honours rate limits, pausing only the kind of data a limit names. A panic
  in your `before_send` is skipped, and memory, payload sizes and shutdown
  time have strict limits.
- **OpenTelemetry-native.** It speaks the Fixwire protocol
  (OpenTelemetry's OTLP/HTTP plus a few small JSON endpoints): errors,
  messages and spans travel as OTLP JSON, with structured stack traces,
  breadcrumbs and redaction on top.
- **Trace headers only where you allow.** Outgoing requests carry trace
  headers only to the hosts and URLs you list, none by default.
- **Your data stays in Europe.** Fixwire stores what your app sends in
  Europe.
- **Made for Rust.** No `unsafe` code and no async runtime needed: it
  works the same in a CLI, a cron job or a tokio server, and hubs follow
  futures across threads.

## 🧩 Integrations

| Integration | What it does | How to use |
| --- | --- | --- |
| tower (`tower` feature) | A layer for axum, hyper and tonic servers: a scope per request, a server span continuing the caller's W3C trace, a release health session, and panics reported with all of that | [Servers](https://github.com/fixwire/fixwire-rust#servers-tower-and-axum) |
| axum (`axum` feature) | The tower layer, with each request named after the route it matched (`GET /orders/{id}`) | [Servers](https://github.com/fixwire/fixwire-rust#servers-tower-and-axum) |
| tracing (`tracing` feature) | A `tracing-subscriber` layer: events become breadcrumbs, error events become Fixwire events | [Logging with tracing](https://github.com/fixwire/fixwire-rust#logging-with-tracing) |
| Any HTTP client | A client span, trace headers for your targets, and an `http` breadcrumb per outgoing request | [`OutgoingRequest`](https://github.com/fixwire/fixwire-rust#traces) |

### Servers: tower and axum

```toml
[dependencies]
fixwire = { version = "0.1", features = ["axum"] } # or "tower" for hyper and tonic
```

The layer gives each request a fork of the current hub with a scope of its
own: the request is on it, without cookies or `Authorization` unless
`send_default_pii` is on. Add it with `Router::layer`, after the routes, so
requests are named after the route they matched:

```rust
async fn order() -> &'static str {
    "ok"
}

let app: axum::Router = axum::Router::new()
    .route("/orders/{id}", axum::routing::get(order))
    .layer(fixwire::tower::FixwireLayer::new())
    // answers 500 to what panicked, once Fixwire has reported it
    .layer(tower_http::catch_panic::CatchPanicLayer::new());
```

With a `release`, each request the layer serves is a release health
session, sent about every minute.

### Logging with tracing

```toml
[dependencies]
fixwire = { version = "0.1", features = ["tracing"] }
```

The layer makes `tracing` events breadcrumbs (info and up) and error
events Fixwire events. An event's `error` field is sent as the exception,
with its chain of sources and the stack where it was logged:

```rust
use tracing_subscriber::prelude::*;

tracing_subscriber::registry()
    .with(fixwire::tracing::layer()) // or .event_level(fixwire::Level::Warning)
    .init();

if let Err(e) = "4x".parse::<u32>() {
    tracing::error!(error = &e as &dyn std::error::Error, "reading the order failed");
}
```

## ⚙️ Configuration

`fixwire::Options` holds every option; set the ones you need and leave the
rest to `..Default::default()`.

| Option | Default | What it does |
| --- | --- | --- |
| `dsn` | `FIXWIRE_DSN` | Where to send: `https://<publishable key>@<host>`. Without one, the SDK does nothing. |
| `release` | `FIXWIRE_RELEASE` | The app's version, such as `api@1.4.0` or a commit SHA. Turns on release health. |
| `environment` | `FIXWIRE_ENVIRONMENT`, else `production` | Where the app runs: `production`, `staging`, … |
| `server_name` | the host name | The machine's name. |
| `service_name` | `OTEL_SERVICE_NAME`, else `api` of `api@1.4.0` | The service's name. |
| `sample_rate` | `1.0` | The share of errors and messages sent. |
| `traces_sample_rate` | `0.0` | The share of new traces kept (0: no tracing). |
| `trace_propagation_targets` | none | The URLs outgoing requests carry trace headers to. |
| `error_budget` | 10 per issue at once, then 1 a minute; 600 a minute in all | Bounds what a crash loop sends. |
| `before_send` | none | Changes an error or message before it is sent, or drops it. |
| `before_breadcrumb` | none | Changes a breadcrumb before it is kept, or drops it. |
| `max_breadcrumbs` | `100` | The breadcrumbs kept per scope. |
| `send_default_pii` | `false` | Sends the user's IP address and request headers that may identify them. |
| `redact` | `true` | Masks secrets and personal data on the device. |
| `sensitive_keys` | built-in list | Replaces the key fragments (`password`, `token`, `cookie`, …) whose values are filtered whole. |
| `max_value_length` | `1024` | The longest string sent, in bytes of UTF-8. |
| `max_stack_frames` | `100` | The frames sent per exception; the newest are kept. |
| `context_lines` | `5` | Source lines read above and below each of the app's frames (at most 5; 0 turns it off). |
| `project_root` | the working directory | Frames under it are the app's, named relative to it. |
| `in_app_include` | none | Module path prefixes whose frames are the app's (`my_crate::`); your binary's crate already is. |
| `in_app_exclude` | none | Module path prefixes whose frames are not the app's. |
| `capture_panics` | `true` | Reports panics; the panic hook that was there still runs. |
| `max_queue` | `100` | The requests waiting to be sent; past it, new data is dropped. |
| `timeout` | 30 s | How long a request to Fixwire may take. |
| `debug` | `FIXWIRE_DEBUG` | Logs what the SDK does to stderr. |
| `auto_session_tracking` | `true` | With a release, counts each request the tower layer serves for release health. |
| `session_interval` | 60 s | How often release health sessions are sent. |

### Sampling

`sample_rate` keeps a share of errors and messages; the error budget
already bounds a crash loop, so most apps keep it at 1. `traces_sample_rate`
keeps a share of new traces. A trace continued from a caller (a
`traceparent` header) follows the caller's decision, and the decision for
a new trace comes from its trace id the same way in every Fixwire SDK, so
services agree.

### Trace propagation targets

Trace headers (`traceparent`, `tracestate`, `baggage`) go only where
`trace_propagation_targets` says, nowhere by default. URLs are compared
without their user info, query and fragment:

- a target with `://` matches URLs that start with it
  (`https://api.example.com/v2`);
- a target starting with `/` matches relative URLs whose path starts with
  it;
- any other target is a host, with a port if it has one: `example.com`
  matches `example.com` and `api.example.com`, not `badexample.com` or
  `example.com.evil.net`.

```rust
let _fixwire = fixwire::init(fixwire::Options {
    traces_sample_rate: 0.2,
    trace_propagation_targets: vec!["inventory.internal".into(), "https://api.example.com/v2".into()],
    ..Default::default()
});
```

### Filtering with before_send

`before_send` sees each error and message last and may change or drop it;
`before_breadcrumb` does the same for breadcrumbs. A callback that panics
is skipped (the debug log says so), and the event goes as it was.

```rust
use std::sync::Arc;

let _fixwire = fixwire::init(fixwire::Options {
    before_send: Some(Arc::new(|mut event: fixwire::Event| {
        if event.transaction.as_deref() == Some("GET /health") {
            return None; // dropped
        }
        event.tags.insert("team".into(), "payments".into());
        Some(event)
    })),
    before_breadcrumb: Some(Arc::new(|crumb: fixwire::Breadcrumb| {
        (crumb.category.as_deref() != Some("sql")).then_some(crumb)
    })),
    ..Default::default()
});
```

### Redaction

Every string the SDK sends from your app's data is masked on the device
with the same rules as the Fixwire server: messages, attributes, span
names and status messages, breadcrumbs, feedback, URLs and their queries,
and the keys of maps. A masked value reads `[REDACTED:email]`,
`[REDACTED:credit_card]` and so on, and the values of sensitive keys
(`password`, `token`, `cookie`, …, or your own `sensitive_keys`) are
filtered whole. Your app's own configuration (release, environment, server
and service names, monitor slugs) is sent as given: masking
`api@1.2.3.example` as an email would break release health.
`redact: false` turns masking off.

### The error budget

Each issue (a cheap fingerprint of the event) may send a burst of
`per_issue_burst` events (10), then `per_issue_per_minute` (1), within
`per_minute` (600) for all issues. Occurrences held back are counted and
ride on the issue's next event, so issue counts stay right.
`ErrorBudget { disabled: true, ..Default::default() }` sends every event.

### Limits

The SDK bounds what it sends and holds, the same way in every Fixwire SDK:

- Strings are cut to `max_value_length` bytes on a character boundary,
  ending in `...`. Masking runs first, over the part kept and the next
  16 kB.
- Values you give (extras, contexts, breadcrumb data, span attributes) go
  at most 10 levels deep and 100 items wide, with at most 10,000
  containers walked.
- An error chain holds at most 10 errors, each with its newest
  `max_stack_frames` frames. Source lines come from regular files of at
  most 10 MB, through a cache of at most 64 files and 32 MB.
- An error or a message is at most 1 MB: over it, it goes without its
  breadcrumbs, then without its contexts, or not at all.
- A segment keeps at most 1,000 child spans, a span at most 128
  attributes; spans go in requests of at most 100 spans and 5 MB.
- A request is sent at most 4 times, waiting about 1 s, then twice as long
  each time; one whose next try would be more than 5 minutes away is
  dropped. Redirects are not followed.
- An incoming `tracestate` over 512 bytes or `baggage` over 8,192 bytes,
  or either with a control character other than tab, is not passed on.

## 🧪 Examples

Real programs reporting to Fixwire. Their tests run them against a fake
ingest and check what Fixwire receives, so they keep working.

- [shop-api](https://github.com/fixwire/fixwire-rust/blob/main/examples/shop-api/main.rs):
  the tower layer on axum, with a scope, a session and a server span per
  request, handled errors with context, a panic reported as a crash, a
  traced call to another service and `tracing` records as breadcrumbs.
- [nightly-report](https://github.com/fixwire/fixwire-rust/blob/main/examples/nightly-report/main.rs):
  a cron job with check-ins to a monitor, one scope per account, a summary
  warning and a trace for the run.

```sh
FIXWIRE_DSN=https://<key>@<host> cargo run --bin shop-api
FIXWIRE_DSN=https://<key>@<host> cargo run --bin nightly-report
```

## 📚 Documentation

The full guide lives in this README and the examples.

- [Configuration](https://github.com/fixwire/fixwire-rust#%EF%B8%8F-configuration)
- [Examples](https://github.com/fixwire/fixwire-rust/tree/main/examples)
- [Changelog](https://github.com/fixwire/fixwire-rust/blob/main/CHANGELOG.md)
- [Security policy](https://github.com/fixwire/fixwire-rust/blob/main/SECURITY.md)
- [Contributing guide](https://github.com/fixwire/fixwire-rust/blob/main/CONTRIBUTING.md)

## 🚧 Coming from another error tracker?

The API follows the shape most error-tracking SDKs share: `init` with
options, `capture_error` and `capture_message`, `set_user` and `set_tag`,
`add_breadcrumb`, and spans with `start_span` and `trace`. Moving over is
mostly a change of crate and DSN. A few things are Rust's own: `init`
returns a guard to keep until the program ends, `capture_error` takes any
`std::error::Error` and walks its `source()` chain, futures carry their hub
with `bind_hub`, and trace headers go nowhere until you list targets.

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
