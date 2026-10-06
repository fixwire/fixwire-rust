# Changelog

All notable changes to the Fixwire SDK for Rust are listed here. Versions follow [Semantic
Versioning](https://semver.org); before 1.0, a minor version may change the
API.

## [Unreleased]

- A panic or a `tracing` event inside `configure_scope` no longer deadlocks the thread: it is reported without the scope. `with_scope` runs `configure` without holding the scope.
- The error budget reads the first 1 KB of a message: a long hostile message no longer costs the calling thread seconds.
- A huge `Retry-After` or `Fixwire-Rate-Limits` no longer stops the sending thread: pauses are cut to a day.
- Requests waiting to be retried are bounded by `max_queue`, and `Client::close` (and the guard) returns within its timeout when Fixwire doesn't answer.
- Redirects from Fixwire are not followed.
- Errors and messages over 1 MB are sent without their breadcrumbs, extra details and source lines, or dropped.
- Release health keeps at most 5000 users between sends; more are counted without their id.
- An incoming `tracestate` or `baggage` over 8 KB, or with control characters, is not passed on.
- Source lines are read from regular files only.
- The `tracing` layer leaves alone what libraries log on the SDK's own threads; `capture_error` takes no stack when the SDK is off.

## [0.1.0] - 2026-10-06

First release.

- Errors with their chain of sources and the stack where they were captured, panics as crashes (the previous panic hook still runs), messages, breadcrumbs and scopes; hubs per thread, carried by futures (`bind_hub`).
- Traces with W3C trace context (`trace`, `trace_future`, `start_span`), and outgoing requests from any HTTP client (`OutgoingRequest`).
- The `tower` and `axum` features: a layer giving each request its scope, a server span named after its route, and a release health session.
- The `tracing` feature: events as breadcrumbs, error events as Fixwire events.
- Cron monitors (`with_monitor`) and feedback.
- On-device redaction with the server's rules; an error budget for crash loops; rate limits honoured per kind of data.
- Examples run against a fake ingest in CI: an axum API and a cron job.
