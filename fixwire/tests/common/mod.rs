//! A fake ingest: an HTTP server that records what the SDK sends (gzip
//! undone, JSON parsed) and answers as told.

#![allow(dead_code)] // each test file uses part of it

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flate2::read::GzDecoder;
use serde_json::Value;

/// One request the SDK made.
#[derive(Clone, Debug)]
pub struct Received {
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

impl Received {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// An answer to give: status and headers.
pub type Answer = (u16, Vec<(&'static str, String)>);

#[derive(Clone, Default)]
pub struct Ingest {
    pub port: u16,
    received: Arc<Mutex<Vec<Received>>>,
    answers: Arc<Mutex<VecDeque<Answer>>>,
}

impl Ingest {
    pub fn start() -> Ingest {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
        let ingest = Ingest {
            port: listener.local_addr().unwrap().port(),
            ..Ingest::default()
        };
        let server = ingest.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let server = server.clone();
                std::thread::spawn(move || server.serve(stream));
            }
        });
        ingest
    }

    pub fn dsn(&self) -> String {
        format!("http://fw_pk_test_rust@127.0.0.1:{}", self.port)
    }

    /// The next requests get these answers, in turn (then 200).
    pub fn answer(&self, answers: impl IntoIterator<Item = Answer>) {
        self.answers.lock().unwrap().extend(answers);
    }

    pub fn received(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }

    pub fn on(&self, path: &str) -> Vec<Received> {
        self.received()
            .into_iter()
            .filter(|r| r.path.starts_with(path))
            .collect()
    }

    /// Waits until `n` requests reached `path`, or two seconds.
    pub fn wait_for(&self, path: &str, n: usize) -> Vec<Received> {
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            let got = self.on(path);
            if got.len() >= n || Instant::now() > until {
                return got;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The log records sent: errors and messages.
    pub fn records(&self) -> Vec<Value> {
        self.on("/v1/logs")
            .iter()
            .flat_map(|r| {
                r.body["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .collect()
    }

    /// The spans sent.
    pub fn spans(&self) -> Vec<Value> {
        self.on("/v1/traces")
            .iter()
            .flat_map(|r| {
                r.body["resourceSpans"][0]["scopeSpans"][0]["spans"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .collect()
    }

    fn serve(&self, stream: TcpStream) {
        let mut writer = stream.try_clone().unwrap();
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
            let mut headers = Vec::new();
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).unwrap();
                let h = h.trim_end();
                if h.is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    headers.push((k.trim().to_owned(), v.trim().to_owned()));
                }
            }
            let length: usize = headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, v)| v.parse().ok())
                .unwrap_or(0);
            let mut raw = vec![0; length];
            reader.read_exact(&mut raw).unwrap();
            let gzip = headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("content-encoding") && v == "gzip");
            let mut text = String::new();
            if gzip {
                GzDecoder::new(&raw[..]).read_to_string(&mut text).unwrap();
            } else {
                text = String::from_utf8_lossy(&raw).into_owned();
            }
            let body = serde_json::from_str(&text).unwrap_or(Value::Null);
            self.received.lock().unwrap().push(Received {
                path,
                headers,
                body,
            });
            let (status, extra) = self
                .answers
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or((200, Vec::new()));
            let mut response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: 2\r\n"
            );
            for (k, v) in extra {
                response.push_str(&format!("{k}: {v}\r\n"));
            }
            response.push_str("\r\n{}");
            if writer.write_all(response.as_bytes()).is_err() {
                return;
            }
        }
    }
}

/// A log record's or span's attributes as a JSON object, values unwrapped.
pub fn attrs(item: &Value) -> serde_json::Map<String, Value> {
    item["attributes"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|kv| (kv["key"].as_str().unwrap().to_owned(), plain(&kv["value"])))
                .collect()
        })
        .unwrap_or_default()
}

/// An OTLP `AnyValue` as plain JSON.
pub fn plain(v: &Value) -> Value {
    if let Some(s) = v.get("stringValue") {
        return s.clone();
    }
    if let Some(b) = v.get("boolValue") {
        return b.clone();
    }
    if let Some(i) = v.get("intValue") {
        return i
            .as_str()
            .and_then(|s| s.parse::<i64>().ok())
            .map(Value::from)
            .unwrap_or(Value::Null);
    }
    if let Some(d) = v.get("doubleValue") {
        return d.clone();
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
