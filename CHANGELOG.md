# Changelog

All notable changes to the Fixwire SDK for Rust are listed here. Versions follow [Semantic
Versioning](https://semver.org); before 1.0, a minor version may change the
API.

## [0.1.0] - 2026-10-06

First release.

- Errors with their chain of sources and the stack where they were captured, panics as crashes (the previous panic hook still runs), messages, breadcrumbs and scopes; hubs per thread, carried by futures (`bind_hub`).
- Traces with W3C trace context (`trace`, `trace_future`, `start_span`), and outgoing requests from any HTTP client (`OutgoingRequest`).
- The `tower` and `axum` features: a layer giving each request its scope, a server span named after its route, and a release health session.
- The `tracing` feature: events as breadcrumbs, error events as Fixwire events.
- Cron monitors (`with_monitor`) and feedback.
- On-device redaction with the server's rules; an error budget for crash loops; rate limits honoured per kind of data.
- Examples run against a fake ingest in CI: an axum API and a cron job.
