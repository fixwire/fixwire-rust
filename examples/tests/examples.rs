//! Runs the examples against a fake ingest (and a fake inventory service)
//! and checks what Fixwire receives, so they keep working.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flate2::read::GzDecoder;
use serde_json::{Map, Value, json};

/// A request a recorder got: its path, headers and JSON body.
type Got = (String, BTreeMap<String, String>, Value);

/// A server recording what it is sent; `answer` picks each response's
/// status from the path.
#[derive(Clone)]
struct Recorder {
    port: u16,
    got: Arc<Mutex<Vec<Got>>>,
}

impl Recorder {
    fn start(answer: fn(&str) -> u16) -> Recorder {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let r = Recorder {
            port: listener.local_addr().unwrap().port(),
            got: Arc::default(),
        };
        let rec = r.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let rec = rec.clone();
                std::thread::spawn(move || rec.serve(stream, answer));
            }
        });
        r
    }

    fn serve(&self, stream: TcpStream, answer: fn(&str) -> u16) {
        let mut out = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let path = line
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_owned();
            let mut headers = BTreeMap::new();
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).unwrap();
                match h.trim_end().split_once(':') {
                    Some((k, v)) => {
                        headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned())
                    }
                    None => break,
                };
            }
            let length = headers
                .get("content-length")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let mut raw = vec![0; length];
            reader.read_exact(&mut raw).unwrap();
            let mut text = String::new();
            if headers.get("content-encoding").map(String::as_str) == Some("gzip") {
                GzDecoder::new(&raw[..]).read_to_string(&mut text).unwrap();
            } else {
                text = String::from_utf8_lossy(&raw).into_owned();
            }
            let status = answer(&path);
            self.got.lock().unwrap().push((
                path,
                headers,
                serde_json::from_str(&text).unwrap_or(Value::Null),
            ));
            let reply = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: 2\r\nContent-Type: application/json\r\n\r\n{{}}"
            );
            if out.write_all(reply.as_bytes()).is_err() {
                return;
            }
        }
    }

    fn on(&self, prefix: &str) -> Vec<Got> {
        self.got
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, ..)| p.starts_with(prefix))
            .cloned()
            .collect()
    }

    fn records(&self) -> Vec<Value> {
        self.on("/v1/logs")
            .into_iter()
            .flat_map(|(_, _, b)| {
                b["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .collect()
    }

    fn spans(&self) -> Vec<Value> {
        self.on("/v1/traces")
            .into_iter()
            .flat_map(|(_, _, b)| {
                b["resourceSpans"][0]["scopeSpans"][0]["spans"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .collect()
    }
}

fn ingest() -> Recorder {
    Recorder::start(|_| 200)
}

fn dsn(ingest: &Recorder) -> String {
    format!("http://fw_pk_test_examples@127.0.0.1:{}", ingest.port)
}

/// A log record's or span's attributes, values unwrapped.
fn attrs(item: &Value) -> Map<String, Value> {
    item["attributes"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|kv| (kv["key"].as_str().unwrap().to_owned(), plain(&kv["value"])))
                .collect()
        })
        .unwrap_or_default()
}

fn plain(v: &Value) -> Value {
    for k in ["stringValue", "boolValue", "doubleValue"] {
        if let Some(x) = v.get(k) {
            return x.clone();
        }
    }
    if let Some(i) = v.get("intValue") {
        return i
            .as_str()
            .and_then(|s| s.parse::<i64>().ok())
            .map(Value::from)
            .unwrap_or(Value::Null);
    }
    if let Some(a) = v.get("arrayValue") {
        return Value::Array(
            a["values"]
                .as_array()
                .map(|x| x.iter().map(plain).collect())
                .unwrap_or_default(),
        );
    }
    if let Some(kv) = v.get("kvlistValue") {
        return Value::Object(
            kv["values"]
                .as_array()
                .map(|x| {
                    x.iter()
                        .map(|e| (e["key"].as_str().unwrap().to_owned(), plain(&e["value"])))
                        .collect()
                })
                .unwrap_or_default(),
        );
    }
    Value::Null
}

/// Starts an example as one the system can stop: on Windows, in a process group of its own, so
/// Ctrl-Break reaches it alone.
fn stoppable(command: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    command
}

/// Stops an example as the system stops a program: SIGTERM, or Ctrl-Break on Windows.
fn stop(app: &std::process::Child) {
    #[cfg(unix)]
    let sent = Command::new("kill")
        .args(["-TERM", &app.id().to_string()])
        .status()
        .is_ok_and(|s| s.success());
    #[cfg(windows)]
    // SAFETY: a plain call with two integers; the process group is the example's own.
    let sent = unsafe {
        use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
        GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, app.id()) != 0
    };
    assert!(sent, "the example was told to stop");
}

#[test]
fn shop_api() {
    let ingest = ingest();
    // The inventory holds sku_1; sku_2 is sold out.
    let inventory = Recorder::start(|path| if path.contains("sku_2") { 409 } else { 200 });
    let mut app = stoppable(&mut Command::new(env!("CARGO_BIN_EXE_shop-api")))
        .env("FIXWIRE_DSN", dsn(&ingest))
        .env(
            "INVENTORY_URL",
            format!("http://127.0.0.1:{}", inventory.port),
        )
        .env("PORT", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(app.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let port: u16 = line
        .trim()
        .trim_start_matches("listening on ")
        .parse()
        .unwrap();

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let call = |method: &str, path: &str, user: &str, body: &str| -> u16 {
        let url = format!("http://127.0.0.1:{port}{path}");
        let res = if method == "GET" {
            agent.get(&url).header("x-user-id", user).call()
        } else {
            agent
                .post(&url)
                .header("x-user-id", user)
                .header("content-type", "application/json")
                .send(body)
        };
        res.map(|r| r.status().as_u16()).unwrap_or(0)
    };
    for (method, path, user, body, want) in [
        ("GET", "/products/sku_1", "", "", 200),
        ("GET", "/products/nope", "", "", 404),
        (
            "POST",
            "/orders",
            "user-1",
            r#"{"sku":"sku_1","card":"4242424242424242"}"#,
            201,
        ),
        (
            "POST",
            "/orders",
            "user-2",
            r#"{"sku":"sku_1","card":"4000000000000002"}"#,
            402,
        ),
        (
            "POST",
            "/orders",
            "user-3",
            r#"{"sku":"sku_2","card":"4242424242424242"}"#,
            409,
        ),
        ("GET", "/admin/report", "", "", 500),
    ] {
        assert_eq!(call(method, path, user, body), want, "{method} {path}");
    }
    stop(&app);
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = app.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "the API did not stop");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "the API stopped cleanly: {status}");

    // Three errors: the declined payment and the sold-out item (handled), the panic (a crash). No 404.
    let records = ingest.records();
    let by = |tx: &str| {
        records
            .iter()
            .filter(|r| attrs(r)["fixwire.transaction"] == tx)
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(records.len(), 3, "{records:#?}");
    let orders = by("POST /orders");
    assert_eq!(orders.len(), 2);
    let payment = orders
        .iter()
        .find(|r| attrs(r)["user.id"] == "user-2")
        .unwrap();
    let a = attrs(payment);
    assert_eq!(a["exception.type"], "ChargeFailed");
    assert_eq!(a["fixwire.exceptions"][1]["type"], "PaymentDeclined");
    assert_eq!(a["fixwire.contexts"]["order"]["sku"], "sku_1");
    assert_eq!(a["fixwire.tags"], json!({"sku": "sku_1"}));
    let crumbs = a["fixwire.breadcrumbs"].as_array().unwrap();
    assert!(
        crumbs.iter().any(|c| c["message"] == "order received"),
        "{crumbs:?}"
    );
    let sold_out = orders
        .iter()
        .find(|r| attrs(r)["user.id"] == "user-3")
        .unwrap();
    assert_eq!(
        attrs(sold_out)["exception.message"],
        "reserving sku_2: inventory answered 409"
    );

    let crash = &by("GET /admin/report")[0];
    let a = attrs(crash);
    assert_eq!(
        (
            crash["severityNumber"].as_i64(),
            a["fixwire.handled"].clone()
        ),
        (Some(21), json!(false))
    );
    assert_eq!(a["exception.message"], "attempt to divide by zero");
    let frames = a["fixwire.exceptions"][0]["frames"].as_array().unwrap();
    let newest = frames
        .iter()
        .rev()
        .find(|f| f["in_app"] == json!(true))
        .unwrap();
    assert_eq!(
        newest["module"], "shop_api::report",
        "where it panicked: {newest}"
    );
    assert_eq!(newest["file"], "shop-api/main.rs");

    // Spans named after their routes; the inventory got the trace, nobody else did.
    let names: Vec<String> = ingest
        .spans()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_owned())
        .collect();
    for want in [
        "GET /products/{id}",
        "POST /orders",
        "GET /admin/report",
        "SELECT products",
    ] {
        assert!(names.iter().any(|n| n == want), "{want} in {names:?}");
    }
    assert!(
        names
            .iter()
            .any(|n| n.starts_with("POST http://127.0.0.1:")),
        "the inventory call: {names:?}"
    );
    let reservations = inventory.on("/reservations");
    assert_eq!(reservations.len(), 3);
    assert!(
        reservations
            .iter()
            .all(|(_, h, _)| h.get("traceparent").is_some_and(|t| t.starts_with("00-")))
    );

    // Release health: every request counted, the crash as a crash.
    let sessions = ingest.on("/v1/sessions");
    let sum = |k: &str| {
        sessions
            .iter()
            .flat_map(|(_, _, b)| b["aggregates"].as_array().cloned().unwrap_or_default())
            .map(|a| a[k].as_u64().unwrap())
            .sum::<u64>()
    };
    assert_eq!((sum("exited"), sum("errored"), sum("crashed")), (3, 2, 1));
}

#[test]
fn nightly_report() {
    let ingest = ingest();
    let out = Command::new(env!("CARGO_BIN_EXE_nightly-report"))
        .env("FIXWIRE_DSN", dsn(&ingest))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "the job fails with 1 of 3 accounts"
    );

    let check_ins = ingest.on("/v1/check-ins/nightly-report");
    assert_eq!(check_ins.len(), 2);
    let (start, end) = (&check_ins[0].2, &check_ins[1].2);
    assert_eq!(start["status"], "in_progress");
    assert_eq!(start["monitor_config"]["schedule"]["value"], "0 3 * * *");
    assert_eq!(start["monitor_config"]["timezone"], "Europe/Berlin");
    assert_eq!(
        (end["status"].clone(), end["check_in_id"].clone()),
        (json!("error"), start["check_in_id"].clone())
    );

    let records = ingest.records();
    assert_eq!(records.len(), 2);
    let failed = records
        .iter()
        .find(|r| r["eventName"] == "exception")
        .unwrap();
    let a = attrs(failed);
    assert_eq!(a["fixwire.tags"], json!({"account": "acct_2"}));
    assert_eq!(a["user.email"], "[REDACTED:email]");
    assert_eq!(
        a["fixwire.exceptions"][1]["message"],
        "partition 2026-10 not found"
    );
    let summary = records
        .iter()
        .find(|r| r["eventName"] == "fixwire.message")
        .unwrap();
    assert_eq!(summary["body"]["stringValue"], "1 of 3 reports failed");
    assert_eq!(summary["severityNumber"], 13);
    assert!(
        attrs(summary).get("fixwire.breadcrumbs").is_none(),
        "each account's scope kept its breadcrumbs"
    );

    let spans = ingest.spans();
    let run = spans
        .iter()
        .find(|s| s["name"] == "nightly-report")
        .unwrap();
    assert_eq!(
        spans
            .iter()
            .filter(|s| s["parentSpanId"] == run["spanId"])
            .count(),
        3,
        "a span per account"
    );
    assert_eq!(failed["traceId"], run["traceId"]);
}
