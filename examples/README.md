# Examples

Real programs reporting to Fixwire. `tests/examples.rs` runs them against a
fake ingest and checks what Fixwire receives, so they keep working
(`cargo test -p fixwire-examples`).

| Example | Shows |
|---|---|
| [shop-api](shop-api/main.rs) | The tower layer on axum: a scope, a session and a server span per request named after its route; the signed-in user; handled errors with context and their cause; a panic reported as a crash (answered 500 by tower-http's panic layer); 404s not reported; a database span; a traced call to another service with trace headers sent only to it; `tracing` records as breadcrumbs; finishing when stopped (SIGTERM, Ctrl-C, or Ctrl-Break on Windows) |
| [nightly-report](nightly-report/main.rs) | A cron job: check-ins to a monitor (created from the first one), one scope per account, carrying on after a failure, a summary warning, a trace for the run, exiting with the run's result |

```sh
FIXWIRE_DSN=https://<key>@<host> cargo run --bin shop-api
FIXWIRE_DSN=https://<key>@<host> cargo run --bin nightly-report
```
