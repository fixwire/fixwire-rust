//! What the SDK sends, checked at a fake ingest: errors with their chains,
//! frames and scope, messages, spans, check-ins, feedback, and how it backs
//! off when Fixwire asks it to.

mod common;

use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use common::{Ingest, attrs};
use fixwire::{Breadcrumb, Client, Feedback, Hub, Level, MonitorConfig, Options, Scope, User};
use serde_json::{Value, json};

/// A hub of its own, sending to `ingest`: tests run side by side.
fn hub(ingest: &Ingest, opts: Options) -> Hub {
    let client = Client::new(Options {
        dsn: Some(ingest.dsn()),
        release: Some("shop@1.0.0".into()),
        project_root: Some(env!("CARGO_MANIFEST_DIR").into()),
        ..opts
    })
    .unwrap();
    Hub::new(Some(Arc::new(client)), Scope::default())
}

fn flush(hub: &Hub) {
    assert!(hub.flush(Duration::from_secs(5)), "flushed in time");
}

#[derive(Debug)]
struct Declined {
    card: &'static str,
}
impl fmt::Display for Declined {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "card {} declined", self.card)
    }
}
impl Error for Declined {}

#[derive(Debug)]
struct ChargeFailed(Declined);
impl fmt::Display for ChargeFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("charging order 7 failed")
    }
}
impl Error for ChargeFailed {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

fn charge_card() -> Result<(), ChargeFailed> {
    Err(ChargeFailed(Declined {
        card: "4111 1111 1111 1111",
    }))
}

#[test]
fn errors_go_with_their_chain_their_stack_and_the_scope() {
    let ingest = Ingest::start();
    let hub = hub(&ingest, Options::default());
    let id = Hub::run(hub.clone(), || {
        fixwire::set_user(Some(User {
            id: Some("user-1".into()),
            email: Some("ada@example.com".into()),
            ..User::default()
        }));
        fixwire::set_tag("plan", "team");
        fixwire::add_breadcrumb(Breadcrumb::new("cart", "checkout started"));
        let err = charge_card().unwrap_err();
        fixwire::capture_error(&err)
    })
    .expect("sent");
    flush(&hub);

    let records = ingest.records();
    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert_eq!(
        (r["eventName"].as_str(), r["severityNumber"].as_i64()),
        (Some("exception"), Some(17))
    );
    let a = attrs(r);
    assert_eq!(a["fixwire.event_id"], json!(id));
    assert_eq!(
        (
            a["exception.type"].as_str(),
            a["exception.message"].as_str()
        ),
        (Some("ChargeFailed"), Some("charging order 7 failed"))
    );
    let chain = a["fixwire.exceptions"].as_array().unwrap();
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[1]["type"], "Declined");
    assert_eq!(
        chain[1]["message"], "card [REDACTED:credit_card] declined",
        "masked on the device"
    );
    assert_eq!(chain[1]["mechanism"]["type"], "chained");

    // The newest frame of the app is this test, relative to the project, with its source line.
    let frames = chain[0]["frames"].as_array().unwrap();
    let mine = frames
        .iter()
        .rev()
        .find(|f| f["in_app"] == json!(true))
        .expect("an in-app frame");
    assert_eq!(mine["function"], "{closure}");
    assert_eq!(
        mine["module"],
        "events::errors_go_with_their_chain_their_stack_and_the_scope"
    );
    assert_eq!(mine["file"], "tests/events.rs");
    assert!(
        mine["context_line"]
            .as_str()
            .unwrap()
            .contains("capture_error"),
        "{mine}"
    );
    assert!(
        frames.iter().all(|f| !f["module"]
            .as_str()
            .unwrap_or_default()
            .starts_with("fixwire::")),
        "no SDK frames"
    );
    assert!(
        frames.iter().any(|f| f["in_app"] == json!(false)),
        "the standard library's frames are not the app's"
    );

    assert_eq!(a["fixwire.tags"], json!({"plan": "team"}));
    assert_eq!(a["user.id"], "user-1");
    assert_eq!(a["user.email"], "[REDACTED:email]");
    assert_eq!(a["fixwire.breadcrumbs"][0]["message"], "checkout started");
    assert!(a.get("fixwire.handled").is_none(), "handled by default");

    let resource = attrs(&ingest.on("/v1/logs")[0].body["resourceLogs"][0]["resource"]);
    assert_eq!(resource["telemetry.sdk.name"], "fixwire.rust");
    assert_eq!(resource["service.name"], "shop");
    assert_eq!(resource["service.version"], "shop@1.0.0");
    let req = &ingest.on("/v1/logs")[0];
    assert_eq!(req.header("Authorization"), Some("Bearer fw_pk_test_rust"));
    assert_eq!(req.header("Content-Encoding"), Some("gzip"));
}

#[test]
fn messages_scopes_and_before_send() {
    let ingest = Ingest::start();
    let hub = hub(
        &ingest,
        Options {
            before_send: Some(Arc::new(|mut e: fixwire::Event| {
                if e.message.as_deref() == Some("drop me") {
                    return None;
                }
                e.tags.insert("seen".into(), "yes".into());
                Some(e)
            })),
            ..Options::default()
        },
    );
    Hub::run(hub.clone(), || {
        fixwire::with_scope(
            |s| s.set_transaction("nightly-report"),
            || fixwire::capture_message("report sent to ada@example.com", Level::Warning),
        );
        assert!(fixwire::capture_message("drop me", Level::Info).is_none());
    });
    flush(&hub);
    let records = ingest.records();
    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert_eq!(r["eventName"], "fixwire.message");
    assert_eq!(r["severityNumber"], 13);
    assert_eq!(r["body"]["stringValue"], "report sent to [REDACTED:email]");
    let a = attrs(r);
    assert_eq!(a["fixwire.transaction"], "nightly-report");
    assert_eq!(a["fixwire.tags"], json!({"seen": "yes"}));
    let after = Hub::run(hub.clone(), || {
        fixwire::configure_scope(|s| s.span().is_none())
    });
    assert!(after, "with_scope's scope is gone");
}

#[test]
fn a_crash_loop_costs_a_burst_and_a_count() {
    let ingest = Ingest::start();
    let hub = hub(&ingest, Options::default());
    Hub::run(hub.clone(), || {
        for _ in 0..30 {
            fixwire::capture_error(&charge_card().unwrap_err());
        }
    });
    flush(&hub);
    assert_eq!(ingest.records().len(), 10);
}

#[test]
fn spans_go_with_their_segment() {
    let ingest = Ingest::start();
    let hub = hub(
        &ingest,
        Options {
            traces_sample_rate: 1.0,
            ..Options::default()
        },
    );
    Hub::run(hub.clone(), || {
        let request = fixwire::Span::continue_trace(
            Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
            None,
            None,
            "POST /orders",
            "http.server",
            hub.client(),
        );
        fixwire::configure_scope(|s| s.set_span(Some(request.clone())));
        let n = fixwire::trace("SELECT orders", "db.query", |span| {
            span.set_attribute("db.system.name", "postgresql");
            fixwire::trace("encode", "serialize", |_| 3)
        });
        assert_eq!(n, 3);
        assert!(
            ingest.on("/v1/traces").is_empty(),
            "children wait for their segment"
        );
        fixwire::capture_message("in the trace", Level::Info);
        request.finish();
    });
    flush(&hub);
    let spans = ingest.spans();
    assert_eq!(spans.len(), 3);
    let by_name = |n: &str| spans.iter().find(|s| s["name"] == n).unwrap().clone();
    let (root, query, encode) = (
        by_name("POST /orders"),
        by_name("SELECT orders"),
        by_name("encode"),
    );
    assert_eq!(root["traceId"], "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(root["parentSpanId"], "00f067aa0ba902b7");
    assert_eq!(
        root["flags"],
        0x100 | 0x200 | 1,
        "a segment with a remote parent, sampled"
    );
    assert_eq!(query["parentSpanId"], root["spanId"]);
    assert_eq!(encode["parentSpanId"], query["spanId"]);
    assert_eq!(
        (root["kind"].as_i64(), query["kind"].as_i64()),
        (Some(2), Some(3))
    );
    assert_eq!(attrs(&query)["fixwire.op"], "db.query");
    let message = &ingest.records()[0];
    assert_eq!(
        (message["traceId"].clone(), message["spanId"].clone()),
        (root["traceId"].clone(), root["spanId"].clone())
    );
}

#[test]
fn unsampled_traces_send_nothing_but_carry_on() {
    let ingest = Ingest::start();
    let hub = hub(
        &ingest,
        Options {
            traces_sample_rate: 0.0,
            ..Options::default()
        },
    );
    let traceparent = Hub::run(hub.clone(), || {
        fixwire::trace("job", "task", |s| s.traceparent())
    });
    flush(&hub);
    assert!(traceparent.ends_with("-00"));
    assert!(ingest.on("/v1/traces").is_empty());
}

#[test]
fn trace_headers_go_to_the_targets_only() {
    let ingest = Ingest::start();
    let hub = hub(
        &ingest,
        Options {
            traces_sample_rate: 1.0,
            trace_propagation_targets: vec![
                "example.com".into(),
                "https://api.other.io/v2".into(),
                "inventory.internal:8080".into(),
            ],
            ..Options::default()
        },
    );
    let carries = |url: &str| {
        Hub::run(hub.clone(), || {
            fixwire::trace("job", "task", |_| {
                let call = fixwire::OutgoingRequest::start("GET", url);
                let names: Vec<&str> = call.headers().iter().map(|(n, _)| *n).collect();
                let carries = names.contains(&"traceparent");
                call.finish(Some(200), None);
                carries
            })
        })
    };
    for url in [
        "https://example.com/",
        "https://api.EXAMPLE.com/orders?id=7",
        "https://api.other.io/v2/orders",
        "https://ada:pw@api.other.io/v2/x",
        "http://inventory.internal:8080/reservations",
    ] {
        assert!(carries(url), "{url}");
    }
    for url in [
        "https://badexample.com/",
        "https://example.com.evil.net/",
        "https://evil.net/?next=https://example.com/",
        "https://api.other.io/v1/orders",
        "http://inventory.internal:9090/reservations",
        "/orders",
    ] {
        assert!(!carries(url), "{url}");
    }
}

#[test]
fn check_ins_and_feedback() {
    let ingest = Ingest::start();
    let hub = hub(&ingest, Options::default());
    let result: Result<(), std::io::Error> = Hub::run(hub.clone(), || {
        fixwire::with_monitor(
            "nightly report",
            Some(MonitorConfig::crontab("0 3 * * *")),
            || Err(std::io::Error::other("the mail server is down")),
        )
    });
    assert!(result.is_err());
    Hub::run(hub.clone(), || {
        fixwire::set_user(Some(User {
            email: Some("ada@example.com".into()),
            ..User::default()
        }));
        assert!(
            fixwire::capture_feedback(Feedback {
                message: Some("  ".into()),
                ..Feedback::default()
            })
            .is_none()
        );
        fixwire::capture_feedback(Feedback {
            message: Some("The refund was wrong, mail ada@example.com".into()),
            score: Some(-3.0),
            trace_id: Some("4bf92f3577b34da6a3ce929d0e0e4736".into()),
            ..Feedback::default()
        });
    });
    flush(&hub);
    let check_ins = ingest.on("/v1/check-ins/nightly%20report");
    assert_eq!(check_ins.len(), 2);
    let (start, end) = (&check_ins[0].body, &check_ins[1].body);
    assert_eq!(start["status"], "in_progress");
    assert_eq!(
        start["monitor_config"]["schedule"],
        json!({"type": "crontab", "value": "0 3 * * *"})
    );
    assert_eq!(end["status"], "error");
    assert_eq!(end["check_in_id"], start["check_in_id"]);
    assert!(end["duration"].as_f64().unwrap() > 0.0, "a measured run");
    assert!(end.get("monitor_config").is_none());

    let feedback = ingest.on("/v1/feedback");
    assert_eq!(feedback.len(), 1);
    let f = &feedback[0].body;
    assert_eq!(f["score"], -1.0);
    assert_eq!(f["message"], "The refund was wrong, mail [REDACTED:email]");
    assert_eq!(f["email"], "[REDACTED:email]");
    assert_eq!(f["trace_id"], "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(f["sdk"]["name"], "fixwire.rust");

    let ok = Hub::run(hub.clone(), || {
        fixwire::with_monitor("hourly", None, || Ok::<_, std::io::Error>(7))
    });
    assert_eq!(ok.unwrap(), 7);
    flush(&hub);
    assert_eq!(
        ingest.on("/v1/check-ins/hourly").last().unwrap().body["status"],
        "ok"
    );
}

#[test]
fn retries_what_failed_and_pauses_what_is_limited() {
    let ingest = Ingest::start();
    // The first answer fails; the retry goes through.
    ingest.answer([(503, vec![])]);
    let hub = hub(&ingest, Options::default());
    Hub::run(hub.clone(), || {
        fixwire::capture_message("first", Level::Info)
    });
    flush(&hub);
    assert_eq!(ingest.on("/v1/logs").len(), 2, "sent again after a 503");

    // Errors are paused for a minute: what follows waits, feedback still goes.
    ingest.answer([(200, vec![("Fixwire-Rate-Limits", "60:error".into())])]);
    Hub::run(hub.clone(), || {
        fixwire::capture_message("limited", Level::Info);
    });
    ingest.wait_for("/v1/logs", 3);
    Hub::run(hub.clone(), || {
        fixwire::capture_message("waits", Level::Info);
        fixwire::capture_feedback(Feedback {
            score: Some(1.0),
            ..Feedback::default()
        });
    });
    ingest.wait_for("/v1/feedback", 1);
    assert!(
        !hub.flush(Duration::from_millis(300)),
        "the paused event is still waiting"
    );
    assert_eq!(ingest.on("/v1/logs").len(), 3);
    assert_eq!(ingest.on("/v1/feedback").len(), 1);
}

#[test]
fn pauses_past_the_clock_are_cut_to_a_day() {
    let ingest = Ingest::start();
    // Seconds the clock can't add: the sending thread lives on, errors wait a day.
    ingest.answer([(
        200,
        vec![("Fixwire-Rate-Limits", "18446744073709551615:error".into())],
    )]);
    let hub = hub(&ingest, Options::default());
    Hub::run(hub.clone(), || {
        fixwire::capture_message("paused for a day", Level::Info)
    });
    ingest.wait_for("/v1/logs", 1);
    Hub::run(hub.clone(), || {
        // Its next try is a day away, not within 5 minutes: dropped.
        fixwire::capture_message("dropped", Level::Info);
        fixwire::capture_feedback(Feedback {
            score: Some(1.0),
            ..Feedback::default()
        });
    });
    flush(&hub);
    assert_eq!(ingest.on("/v1/logs").len(), 1);
    assert_eq!(ingest.on("/v1/feedback").len(), 1, "other data goes on");
}

#[test]
fn a_5xx_that_says_how_long_pauses_everything() {
    let ingest = Ingest::start();
    ingest.answer([(503, vec![("Retry-After", "1".into())])]);
    let hub = hub(&ingest, Options::default());
    let start = std::time::Instant::now();
    Hub::run(hub.clone(), || {
        fixwire::capture_message("unavailable", Level::Info)
    });
    ingest.wait_for("/v1/logs", 1);
    Hub::run(hub.clone(), || {
        fixwire::capture_feedback(Feedback {
            score: Some(1.0),
            ..Feedback::default()
        })
    });
    flush(&hub);
    assert!(
        start.elapsed() >= Duration::from_secs(1),
        "everything waited"
    );
    assert_eq!(ingest.on("/v1/logs").len(), 2, "tried again");
    assert_eq!(ingest.on("/v1/feedback").len(), 1);

    // A day and a second, or a date two days away, is a day: what waits is dropped.
    for retry_after in ["86401".to_owned(), http_date_in(2 * 86_400)] {
        let ingest = Ingest::start();
        ingest.answer([(429, vec![("Retry-After", retry_after.clone())])]);
        let hub = self::hub(&ingest, Options::default());
        Hub::run(hub.clone(), || {
            fixwire::capture_message("limited", Level::Info);
        });
        ingest.wait_for("/v1/logs", 1);
        Hub::run(hub.clone(), || {
            fixwire::capture_feedback(Feedback {
                score: Some(1.0),
                ..Feedback::default()
            })
        });
        flush(&hub);
        assert_eq!(ingest.on("/v1/logs").len(), 1, "{retry_after}");
        assert!(ingest.on("/v1/feedback").is_empty(), "{retry_after}");
    }
}

/// An HTTP date `secs` from now: `Thu, 08 Oct 2026 10:00:00 GMT`.
fn http_date_in(secs: u64) -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + secs;
    let (days, rem) = ((t / 86_400) as i64, t % 86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let names = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let weekdays = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    format!(
        "{}, {day:02} {} {year} {:02}:{:02}:{:02} GMT",
        weekdays[(days % 7) as usize],
        names[(month - 1) as usize],
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

#[test]
fn redirects_are_not_followed() {
    let elsewhere = Ingest::start();
    let ingest = Ingest::start();
    let location = format!("http://127.0.0.1:{}/v1/logs", elsewhere.port);
    ingest.answer([(307, vec![("Location", location)])]);
    let hub = hub(&ingest, Options::default());
    Hub::run(hub.clone(), || {
        fixwire::capture_message("stays home", Level::Info)
    });
    flush(&hub);
    assert_eq!(ingest.on("/v1/logs").len(), 1);
    assert!(
        elsewhere.received().is_empty(),
        "neither the key nor the data go elsewhere"
    );
}

#[test]
fn closing_takes_no_longer_than_asked() {
    // A server that takes requests and never answers.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _held: Vec<_> = listener.incoming().collect();
    });
    let client = Arc::new(
        Client::new(Options {
            dsn: Some(format!("http://k@127.0.0.1:{port}")),
            error_budget: fixwire::ErrorBudget {
                disabled: true,
                ..Default::default()
            },
            ..Options::default()
        })
        .unwrap(),
    );
    let hub = Hub::new(Some(Arc::clone(&client)), Scope::default());
    Hub::run(hub, || {
        for _ in 0..150 {
            fixwire::capture_message("never answered", Level::Info);
        }
    });
    let start = std::time::Instant::now();
    client.close(Duration::from_millis(200));
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn events_over_a_megabyte_go_without_breadcrumbs_then_contexts_or_not_at_all() {
    let ingest = Ingest::start();
    let hub = hub(
        &ingest,
        Options {
            error_budget: fixwire::ErrorBudget {
                disabled: true,
                ..Default::default()
            },
            ..Options::default()
        },
    );
    // About 2 MB: 100 lists of 20 strings of 1000 bytes.
    let heavy = || -> serde_json::Map<String, Value> {
        (0..100)
            .map(|i| (format!("k{i}"), json!(vec!["x".repeat(1000); 20])))
            .collect()
    };
    let sent = Hub::run(hub.clone(), || {
        let mut sent = Vec::new();
        for _ in 0..100 {
            fixwire::add_breadcrumb(Breadcrumb {
                message: Some("heavy".into()),
                data: (0..20)
                    .map(|i| (i.to_string(), json!("y".repeat(1000))))
                    .collect(),
                ..Breadcrumb::default()
            });
        }
        fixwire::configure_scope(|s| {
            s.set_context(
                "light",
                json!({"plan": "team"}).as_object().unwrap().clone(),
            )
        });
        sent.push(fixwire::capture_message(
            "without breadcrumbs",
            Level::Error,
        ));
        fixwire::configure_scope(|s| s.set_context("heavy", heavy()));
        sent.push(fixwire::capture_message("without contexts", Level::Error));
        fixwire::configure_scope(|s| {
            s.set_context("heavy", serde_json::Map::new());
            s.set_extra("dump", Value::Object(heavy()));
        });
        sent.push(fixwire::capture_message("dropped", Level::Error));
        sent
    });
    flush(&hub);
    assert!(sent[0].is_some() && sent[1].is_some());
    assert_eq!(sent[2], None, "still over without breadcrumbs and contexts");
    let records = ingest.records();
    assert_eq!(records.len(), 2);
    let (first, second) = (attrs(&records[0]), attrs(&records[1]));
    assert!(!first.contains_key("fixwire.breadcrumbs"));
    assert_eq!(
        first["fixwire.contexts"],
        json!({"light": {"plan": "team"}})
    );
    assert!(!second.contains_key("fixwire.breadcrumbs"));
    assert!(!second.contains_key("fixwire.contexts"));
    for r in &records {
        assert!(serde_json::to_vec(r).unwrap().len() <= 1 << 20);
    }
}

#[test]
fn strings_are_cut_after_redaction_and_values_bounded() {
    let ingest = Ingest::start();
    let hub = hub(&ingest, Options::default());
    let (begin, end) = (
        concat!("-----BEGIN RSA PRIVATE ", "KEY-----\n"),
        concat!("\n-----END RSA PRIVATE ", "KEY-----"),
    );
    let pem = format!("{begin}{}{end}", "MIIEpAIBAAKCAQEA".repeat(100));
    Hub::run(hub.clone(), || {
        // 1025 bytes, "é" across the cut.
        let message = format!("{}{}", "a".repeat(1020), "é".repeat(2)) + "a";
        fixwire::capture_message(message, Level::Warning);
        // The cut (at 1024 bytes) goes through the key: still masked whole.
        fixwire::capture_message(format!("{} {pem} tail", "b".repeat(900)), Level::Warning);
        let deep = (0..12).fold(json!("bottom"), |v, _| json!({"next": v}));
        fixwire::configure_scope(|s| s.set_extra("deep", deep));
        fixwire::configure_scope(|s| s.set_extra("wide", json!((0..150).collect::<Vec<_>>())));
        fixwire::capture_message(format!("{}😀", "c".repeat(1022)), Level::Warning);
    });
    flush(&hub);
    let records = ingest.records();
    let body = |i: usize| {
        records[i]["body"]["stringValue"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(body(0), format!("{}...", "a".repeat(1020)));
    assert_eq!(
        body(1),
        format!("{} [REDACTED:private_key] tail", "b".repeat(900)),
        "masked, then short enough"
    );
    // 1026 bytes: the emoji goes, "..." ends it within 1024.
    assert_eq!(body(2), format!("{}...", "c".repeat(1021)));
    let a = attrs(&records[2]);
    let mut deep = &a["deep"];
    for _ in 0..10 {
        deep = &deep["next"];
    }
    assert_eq!(deep, &json!("[Object]"), "ten levels deep");
    assert_eq!(a["wide"].as_array().unwrap().len(), 100);
}

#[test]
fn frames_keep_the_newest_and_failing_callbacks_are_skipped() {
    let ingest = Ingest::start();
    let hub = hub(
        &ingest,
        Options {
            before_send: Some(Arc::new(|e: fixwire::Event| {
                if e.message.as_deref() == Some("before_send panics") {
                    panic!("a bug in before_send");
                }
                Some(e)
            })),
            before_breadcrumb: Some(Arc::new(|b: Breadcrumb| {
                if b.message.as_deref() == Some("kept as it was") {
                    panic!("a bug in before_breadcrumb");
                }
                Some(b)
            })),
            ..Options::default()
        },
    );
    let frames: Vec<fixwire::Frame> = (0..101)
        .map(|i| fixwire::Frame {
            function: format!("f{i}"),
            ..fixwire::Frame::default()
        })
        .collect();
    let chain: Vec<fixwire::Exception> = (0..11)
        .map(|i| fixwire::Exception {
            ty: format!("E{i}"),
            message: "deep".into(),
            frames: frames.clone(),
            ..fixwire::Exception::default()
        })
        .collect();
    let sent = Hub::run(hub.clone(), || {
        fixwire::add_breadcrumb(Breadcrumb::new("app", "kept as it was"));
        let mut event = fixwire::Event::default();
        event.exceptions = chain;
        let a = fixwire::capture_event(event);
        (
            a,
            fixwire::capture_message("before_send panics", Level::Info),
        )
    });
    flush(&hub);
    assert!(sent.0.is_some() && sent.1.is_some(), "sent as they were");
    let records = ingest.records();
    let a = attrs(&records[0]);
    let chain = a["fixwire.exceptions"].as_array().unwrap();
    assert_eq!(chain.len(), 10);
    let frames = chain[0]["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 100);
    assert_eq!(
        (
            frames[0]["function"].as_str(),
            frames[99]["function"].as_str()
        ),
        (Some("f1"), Some("f100")),
        "the oldest goes"
    );
    assert_eq!(a["fixwire.breadcrumbs"][0]["message"], "kept as it was");
    assert_eq!(records[1]["body"]["stringValue"], "before_send panics");
}

#[test]
fn spans_go_in_requests_of_at_most_100_and_5_mb() {
    let ingest = Ingest::start();
    let hub = hub(
        &ingest,
        Options {
            traces_sample_rate: 1.0,
            ..Options::default()
        },
    );
    Hub::run(hub.clone(), || {
        fixwire::trace("job", "task", |job| {
            for i in 0..250 {
                fixwire::trace(format!("step {i}"), "task", |_| ());
            }
            // About 6 MB of attributes: dropped alone.
            fixwire::trace("huge", "task", |s| {
                for i in 0..127 {
                    s.set_attribute(format!("a{i}"), json!(vec!["z".repeat(1000); 50]));
                }
            });
            // 200 attributes: 127 and fixwire.op are kept.
            fixwire::trace("many", "task", |s| {
                for i in 0..200 {
                    s.set_attribute(format!("b{i:03}"), i);
                }
            });
            job.set_attribute("steps", 250);
        });
    });
    flush(&hub);
    let requests = ingest.on("/v1/traces");
    let sizes: Vec<usize> = requests
        .iter()
        .map(|r| {
            r.body["resourceSpans"][0]["scopeSpans"][0]["spans"]
                .as_array()
                .unwrap()
                .len()
        })
        .collect();
    assert_eq!(sizes, [100, 100, 52], "250 steps, many and the job");
    let spans = ingest.spans();
    assert!(spans.iter().all(|s| s["name"] != "huge"));
    let many = spans.iter().find(|s| s["name"] == "many").unwrap();
    let a = attrs(many);
    assert_eq!(a.len(), 128);
    assert!(a.contains_key("fixwire.op") && a.contains_key("b126") && !a.contains_key("b127"));
}

#[test]
fn without_a_dsn_nothing_is_sent() {
    let client = Client::new(Options {
        dsn: Some(String::new()),
        ..Options::default()
    })
    .unwrap();
    if std::env::var("FIXWIRE_DSN").is_err() {
        assert!(!client.is_enabled());
    }
    assert!(
        Client::new(Options {
            dsn: Some("ingest.fixwire.io".into()),
            ..Options::default()
        })
        .is_err()
    );
    let value: Value = json!(null);
    assert!(value.is_null());
}

#[test]
fn the_apps_configuration_is_cut_but_not_redacted() {
    let ingest = Ingest::start();
    let environment = format!("staging-{}", "e".repeat(40));
    let client = Client::new(Options {
        dsn: Some(ingest.dsn()),
        release: Some("api@1.2.3.example".into()),
        environment: Some(environment.clone()),
        server_name: Some("ada@example.com".into()),
        max_value_length: 32,
        traces_sample_rate: 1.0,
        auto_session_tracking: false,
        ..Options::default()
    })
    .unwrap();
    let hub = Hub::new(Some(Arc::new(client)), Scope::default());
    let slug = format!("nightly-{}", "m".repeat(40));
    let timezone = format!("Europe/{}", "z".repeat(40));
    let url = format!("https://shop.example/{}", "p".repeat(40));
    let source = format!("widget-{}", "s".repeat(40));
    Hub::run(hub.clone(), || {
        // The same text in the app's data is masked.
        fixwire::capture_message("api@1.2.3.example", Level::Info);
        fixwire::trace("mail ada@example.com", "task", |_| ());
        fixwire::capture_feedback(Feedback {
            message: Some("wrong".into()),
            name: Some("ada@example.com".into()),
            url: Some(url.clone()),
            source: Some(source.clone()),
            ..Feedback::default()
        });
        let config = MonitorConfig {
            timezone: Some(timezone.clone()),
            ..MonitorConfig::crontab("0 3 * * *")
        };
        fixwire::with_monitor(&slug, Some(config), || Ok::<_, std::io::Error>(()))
    })
    .unwrap();
    flush(&hub);
    // Cut to 32 bytes: 29 and "...".
    let cut = |s: &str| format!("{}...", &s[..29]);

    let logs = ingest.on("/v1/logs");
    assert_eq!(
        ingest.records()[0]["body"]["stringValue"],
        "[REDACTED:email]"
    );
    let resource = attrs(&logs[0].body["resourceLogs"][0]["resource"]);
    assert_eq!(resource["service.version"], "api@1.2.3.example");
    assert_eq!(resource["service.name"], "api");
    assert_eq!(resource["host.name"], "ada@example.com");
    assert_eq!(resource["deployment.environment.name"], cut(&environment));
    assert_eq!(ingest.spans()[0]["name"], "mail [REDACTED:email]");

    let f = &ingest.on("/v1/feedback")[0].body;
    assert_eq!(
        (&f["release"], &f["environment"]),
        (&json!("api@1.2.3.example"), &json!(cut(&environment)))
    );
    assert_eq!(f["name"], "[REDACTED:email]");
    assert_eq!(
        (&f["url"], &f["source"]),
        (&json!(cut(&url)), &json!(cut(&source)))
    );

    let check_ins = ingest.on(&format!("/v1/check-ins/{}", cut(&slug)));
    assert_eq!(check_ins.len(), 2, "the slug, cut");
    assert_eq!(check_ins[0].body["environment"], cut(&environment));
    assert_eq!(
        check_ins[0].body["monitor_config"]["timezone"],
        cut(&timezone)
    );
}
