# Changelog

All notable changes to the Fixwire SDK for Rust are listed here. Versions follow [Semantic
Versioning](https://semver.org); before 1.0, a minor version may change the
API.

## [0.1.1] - 2026-10-06

- `init` no longer panics on an invalid `FIXWIRE_DSN`: an invalid DSN, given or in `FIXWIRE_DSN`, is said on stderr and leaves the SDK off (a valid `FIXWIRE_DSN` no longer stands in for an invalid `dsn`).
- A panic or a `tracing` event inside `configure_scope` no longer deadlocks the thread: it is reported without the scope. `with_scope` runs `configure` without holding the scope.
- The error budget reads the first 1 KB of a message: a long hostile message no longer costs the calling thread seconds.
- A huge `Retry-After` or `Fixwire-Rate-Limits` no longer stops the sending thread: pauses are cut to a day.
- Requests waiting to be retried are bounded by `max_queue`, and `Client::close` (and the guard) returns within its timeout when Fixwire doesn't answer.
- Redirects from Fixwire are not followed.
- Errors and messages over 1 MB are sent without their breadcrumbs, then without their contexts, or dropped.
- Release health keeps at most 5000 users between sends; more are counted without their id. A sessions request holds at most 5000 aggregates.
- An incoming `tracestate` over 512 bytes or `baggage` over 8,192 bytes, or either with a control character other than tab, is not passed on. A `traceparent` is used only in version `00`, in lower-case hex.
- Source lines are read from regular files of at most 10 MB, 5 above and below, through a cache of at most 64 files and 32 MB.
- The `tracing` layer leaves alone what libraries log on the SDK's own threads; `capture_error` takes no stack when the SDK is off.
- Strings are cut to `max_value_length` (new, default 1024) bytes of UTF-8 on a character boundary, ending in `...`; redaction runs first, over the part kept and the next 16 kB, so a secret the cut goes through is still masked. The app's own configuration (release, environment, server and service names, monitor slugs and configs) is cut too, but not redacted.
- Values the app gives (extras, contexts, breadcrumb data, span attributes) are sent at most 10 levels deep and 100 items wide, with at most 10,000 containers walked; `NaN` and infinities from `tracing` fields are the strings `"NaN"`, `"Infinity"` and `"-Infinity"`.
- `max_stack_frames` (new, default 100): the newest frames are kept, for every exception; a chain of at most 10 ends where it comes back to an error already in it.
- `trace_propagation_targets` compares URLs without user info, query and fragment: a target with `://` is a URL prefix, one starting with `/` a relative path prefix, any other a host (and port) matching itself and its subdomains only (`example.com` no longer matches `badexample.com`).
- Redaction follows the server: a secret's name may end a longer one (`access_token`, `client_secret`, `csrfToken`, `PHPSESSID`, `X-Amz-Signature`), with more names and an OAuth `code` in a query or fragment; keys that mask alike are numbered in linear time.
- Spans go in requests of at most 100 spans and 5 MB; a span that can't fit alone is dropped. A span keeps at most 128 attributes.
- A request is sent at most 4 times, a `429`'s retry included; retries wait about 1 s, then twice as long; a request whose next try is more than 5 minutes away is dropped. `Retry-After` may be an HTTP date; a `5xx` with `Retry-After` pauses all data for that long.
- A panic in `before_send`, `before_breadcrumb` or an error's `Display` stays out of the app's way: the callback is skipped (said in the debug log), the message is `[Unreadable]`. A panic in `before_send` while a panic is reported no longer aborts the process. What is logged from a callback is not captured again.
- `capture_feedback`, `start_span` and `OutgoingRequest::start` no longer wait for a scope their own thread is changing.

## [0.1.0] - 2026-10-06

First release.

- Errors with their chain of sources and the stack where they were captured, panics as crashes (the previous panic hook still runs), messages, breadcrumbs and scopes; hubs per thread, carried by futures (`bind_hub`).
- Traces with W3C trace context (`trace`, `trace_future`, `start_span`), and outgoing requests from any HTTP client (`OutgoingRequest`).
- The `tower` and `axum` features: a layer giving each request its scope, a server span named after its route, and a release health session.
- The `tracing` feature: events as breadcrumbs, error events as Fixwire events.
- Cron monitors (`with_monitor`) and feedback.
- On-device redaction with the server's rules; an error budget for crash loops; rate limits honoured per kind of data.
- Examples run against a fake ingest in CI: an axum API and a cron job.
