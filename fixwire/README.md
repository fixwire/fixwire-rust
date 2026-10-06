# fixwire

The Fixwire SDK for Rust: errors and panics, traces, release health, cron
monitors and feedback. Rust 1.88 or newer.

```toml
[dependencies]
fixwire = "0.1"
# a server on axum, hyper or tonic, and tracing:
# fixwire = { version = "0.1", features = ["axum", "tracing"] }
```

```rust,no_run
fn main() {
    let _fixwire = fixwire::init(fixwire::Options {
        dsn: Some("https://fw_pk_live_…@ingest.eu.fixwire.io".into()),
        release: Some("api@1.4.0".into()),
        ..Default::default()
    }); // keep the guard: when it goes, it sends what is left

    if let Err(e) = "4x".parse::<u32>() {
        fixwire::capture_error(&e);
    }
}
```

The DSN is your project's publishable key and the ingest host,
`https://<key>@<host>`. Without the `dsn` option the SDK reads
`FIXWIRE_DSN`; without either it does nothing. `FIXWIRE_RELEASE` and
`FIXWIRE_ENVIRONMENT` work the same way.

**What's different**
- Secrets and personal data are masked on the device, with the same rules
  as the Fixwire server (`redact: false` turns it off), before long strings
  are cut (`max_value_length`, 1024 bytes by default): a secret the cut goes
  through is still masked.
- A crash loop costs a few events and a count, not your quota
  (`error_budget`).
- Captures never block: one thread sends from a bounded queue, retries with
  backoff and honours rate limits, pausing only the kind of data a limit
  names.
- It speaks the Fixwire protocol: errors, messages and spans travel as
  OpenTelemetry's OTLP/HTTP (JSON), with structured stack traces,
  breadcrumbs and redaction on top.

## Errors and panics

`capture_error` sends an error with the stack of the line that captured it,
and its chain of `source()`s; the type is the one you captured
(`ParseIntError`), or read from `Debug` for a `dyn Error`. Panics are
reported as crashes, from the line that panicked, with the panic hook that
was there still running after Fixwire's (`capture_panics: false` turns it
off). Frames of your crate are the app's; the standard library's and your
dependencies' are not.

Release builds carry no debug information by default, so their stacks name
functions only. With limited debug information they name modules, files
and lines too, inlined calls included, at no cost in speed:

```toml
# Cargo.toml
[profile.release]
debug = "limited"
```

```rust,no_run
fixwire::configure_scope(|s| {
    s.set_user(Some(fixwire::User::with_id("user-1")));
    s.set_tag("plan", "team");
});
fixwire::capture_message("the nightly import found no rows", fixwire::Level::Warning);
```

Scopes follow the work: a thread has the process's hub unless
`Hub::run` gives it another, and a future polled with `bind_hub` (the
`tower` layer does this for each request) has its own wherever it is
polled.

## Servers: tower and axum

The `tower` feature's layer gives each request a fork of the current hub
with a scope of its own (the request on it, without cookies or
`Authorization` unless `send_default_pii`), a server span continuing the
caller's W3C trace, and a release health session. With `axum`, a request
is named after the route it matched:

```rust,ignore
let app = axum::Router::new()
    .route("/orders/{id}", axum::routing::get(order))
    .layer(fixwire::tower::FixwireLayer::new())
    // answers 500 to what panicked, once Fixwire has reported it
    .layer(tower_http::catch_panic::CatchPanicLayer::new());
```

## Tracing

With `traces_sample_rate` above 0, spans are sent as OTLP. A trace
continued from a caller follows its decision; a new one is kept the way
every Fixwire SDK decides, so services agree.

```rust,no_run
let rows = fixwire::trace("SELECT orders", "db.query", |span| {
    span.set_attribute("db.system.name", "postgresql");
    42
});
```

`trace_future` does the same for a future. Outgoing requests, from any
HTTP client:

```rust,no_run
let call = fixwire::OutgoingRequest::start("POST", "http://inventory.internal/reservations");
// add call.headers() to the request: trace headers, only for trace_propagation_targets
call.finish(Some(201), None);
```

Trace headers go only where `trace_propagation_targets` says (nowhere by
default). URLs are compared without their user info, query and fragment: a
target with `://` matches URLs that start with it
(`https://api.example.com/v2`), one starting with `/` matches relative URLs
whose path starts with it, and any other is a host, with a port if it has
one: `example.com` matches `example.com` and `api.example.com`, not
`badexample.com` or `example.com.evil.net`.

```rust,no_run
let _fixwire = fixwire::init(fixwire::Options {
    traces_sample_rate: 0.2,
    trace_propagation_targets: vec!["inventory.internal".into(), "https://api.example.com/v2".into()],
    ..Default::default()
});
```

The `tracing` feature's layer makes `tracing` events breadcrumbs (info and
up) and error events Fixwire events, an `error` field the exception:

```rust,ignore
use tracing_subscriber::prelude::*;
tracing_subscriber::registry().with(fixwire::tracing::layer()).init();
```

## Release health, cron monitors, feedback

With a `release`, each request the tower layer serves is a session, sent
about every minute. A scheduled job reports its runs:

```rust,no_run
let result = fixwire::with_monitor(
    "nightly-report",
    Some(fixwire::MonitorConfig::crontab("0 3 * * *")),
    || -> Result<(), std::io::Error> { Ok(()) },
);
```

`capture_feedback` sends what someone said about an error or an AI answer:
a message, a score from -1 to 1, or both.

## License

MIT.
