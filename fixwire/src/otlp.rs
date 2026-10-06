//! OTLP JSON: errors and messages as log records, spans, and the resource
//! that names the app (`fixwire-protocol` §3, §4).

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::client::Client;
use crate::limits::{bounded, bounded_map};
use crate::types::{Event, Level};

/// `v` as an OTLP JSON `AnyValue`.
pub(crate) fn any_value(v: &Value) -> Value {
    match v {
        Value::Null => json!({"stringValue": ""}),
        Value::Bool(b) => json!({"boolValue": b}),
        Value::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => json!({"intValue": i.to_string()}),
            (None, Some(u)) => json!({"intValue": u.to_string()}),
            _ => json!({"doubleValue": n.as_f64().unwrap_or_default()}),
        },
        Value::String(s) => json!({"stringValue": s}),
        Value::Array(items) => {
            json!({"arrayValue": {"values": items.iter().map(any_value).collect::<Vec<_>>()}})
        }
        Value::Object(m) => json!({"kvlistValue": {"values": attributes(m)}}),
    }
}

/// OTLP key-values, in key order; null and empty values are left out.
pub(crate) fn attributes(m: &Map<String, Value>) -> Vec<Value> {
    let mut keys: Vec<&String> = m.keys().collect();
    keys.sort();
    keys.into_iter()
        .filter(|k| !empty(&m[*k]))
        .map(|k| json!({"key": k, "value": any_value(&m[k])}))
        .collect()
}

fn empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

/// Unix time in nanoseconds, as OTLP JSON writes it.
pub(crate) fn nanos(t: SystemTime) -> String {
    t.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string()
}

fn seconds(t: SystemTime) -> f64 {
    let micros = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_micros();
    micros as f64 / 1e6
}

impl Client {
    /// The app, as every OTLP request names it.
    pub(crate) fn resource(&self) -> Value {
        let o = self.options();
        let mut a = Map::new();
        a.insert("service.name".into(), o.service_name.clone().into());
        a.insert("service.version".into(), o.release.clone().into());
        a.insert(
            "deployment.environment.name".into(),
            o.environment.clone().into(),
        );
        a.insert("host.name".into(), o.server_name.clone().into());
        a.insert("telemetry.sdk.name".into(), crate::SDK_NAME.into());
        a.insert("telemetry.sdk.version".into(), crate::SDK_VERSION.into());
        a.insert("telemetry.sdk.language".into(), "rust".into());
        json!({"attributes": attributes(&a)})
    }

    /// An OTLP export of log records or spans, each already JSON: what
    /// `json!({"resourceLogs": [{"resource": …, "scopeLogs": [{"scope": …,
    /// "logRecords": [items]}]}]})` writes, without encoding the items again.
    pub(crate) fn export(&self, kind: Export, items: &[Vec<u8>]) -> Vec<u8> {
        let (head, tail) = self.export_ends(kind);
        let size = items.iter().map(|i| i.len() + 1).sum::<usize>();
        let mut out = Vec::with_capacity(head.len() + size + tail.len());
        out.extend_from_slice(head.as_bytes());
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            out.extend_from_slice(item);
        }
        out.extend_from_slice(tail.as_bytes());
        out
    }

    /// What an export has around its items.
    pub(crate) fn export_ends(&self, kind: Export) -> (String, &'static str) {
        let (resources, scopes, items) = match kind {
            Export::Logs => ("resourceLogs", "scopeLogs", "logRecords"),
            Export::Spans => ("resourceSpans", "scopeSpans", "spans"),
        };
        let head = format!(
            r#"{{"{resources}":[{{"resource":{},"{scopes}":[{{"scope":{},"{items}":["#,
            self.resource(),
            scope()
        );
        (head, "]}]}]}")
    }

    /// An error or a message as a log record (`fixwire-protocol` §4),
    /// redacted, the values the app gave bounded and every string cut to
    /// `max_value_length`.
    pub(crate) fn event_record(&self, e: &Event) -> Value {
        let mut a = Map::new();
        a.insert("fixwire.event_id".into(), e.event_id.clone().into());
        a.insert("fixwire.tags".into(), json!(e.tags));
        a.insert("fixwire.transaction".into(), e.transaction.clone().into());
        a.insert("fixwire.fingerprint".into(), json!(e.fingerprint));
        if let Some(u) = &e.user {
            a.insert("user.id".into(), u.id.clone().into());
            a.insert("user.email".into(), u.email.clone().into());
            a.insert("user.name".into(), u.username.clone().into());
            a.insert("client.address".into(), u.ip_address.clone().into());
        }
        if e.suppressed > 0 {
            a.insert("fixwire.suppressed".into(), e.suppressed.into());
        }
        if !e.contexts.is_empty() {
            let contexts: Map<String, Value> = e
                .contexts
                .iter()
                .map(|(k, v)| (k.clone(), bounded_map(v).into()))
                .collect();
            a.insert("fixwire.contexts".into(), contexts.into());
        }
        for (k, v) in &e.extra {
            a.entry(k.clone()).or_insert_with(|| bounded(v));
        }
        if !e.breadcrumbs.is_empty() {
            let crumbs: Vec<Value> = e
                .breadcrumbs
                .iter()
                .map(|b| {
                    json!({
                        "timestamp": seconds(b.timestamp.unwrap_or(SystemTime::UNIX_EPOCH)),
                        "type": b.ty, "category": b.category, "message": b.message,
                        "level": b.level.unwrap_or_default().as_str(), "data": bounded_map(&b.data),
                    })
                })
                .collect();
            a.insert("fixwire.breadcrumbs".into(), crumbs.into());
        }
        if let Some(r) = &e.request {
            a.insert("http.request.method".into(), r.method.clone().into());
            a.insert("url.full".into(), r.url.clone().into());
            a.insert("url.query".into(), r.query.clone().into());
            a.insert("http.route".into(), r.route.clone().into());
            for (name, v) in &r.headers {
                let key = if name.eq_ignore_ascii_case("user-agent") {
                    "user_agent.original".to_owned()
                } else {
                    format!("http.request.header.{}", name.to_ascii_lowercase())
                };
                a.insert(key, v.clone().into());
            }
        }
        let level = e.level.unwrap_or(Level::Error);
        let mut record = Map::new();
        record.insert(
            "timeUnixNano".into(),
            nanos(e.timestamp.unwrap_or_else(SystemTime::now)).into(),
        );
        record.insert("severityNumber".into(), level.severity().into());
        record.insert(
            "severityText".into(),
            level.as_str().to_ascii_uppercase().into(),
        );
        if let Some((trace, span)) = &e.trace {
            record.insert("traceId".into(), trace.clone().into());
            record.insert("spanId".into(), span.clone().into());
        }
        match e.exceptions.first() {
            None => {
                record.insert("eventName".into(), "fixwire.message".into());
                record.insert(
                    "body".into(),
                    any_value(&self.clean(e.message.as_deref().unwrap_or_default()).into()),
                );
            }
            Some(outer) => {
                record.insert("eventName".into(), "exception".into());
                a.insert("exception.type".into(), outer.ty.clone().into());
                a.insert("exception.message".into(), outer.message.clone().into());
                let chain: Vec<Value> = e
                    .exceptions
                    .iter()
                    .map(|x| {
                        let frames: Vec<Value> = x
                            .frames
                            .iter()
                            .map(|f| {
                                json!({
                                    "function": f.function, "module": f.module, "file": f.file, "abs_path": f.abs_path,
                                    "line": f.line, "column": f.column, "in_app": f.in_app, "context_line": f.context_line,
                                    "pre_context": f.pre_context, "post_context": f.post_context,
                                })
                            })
                            .collect();
                        json!({
                            "type": x.ty, "message": x.message, "module": x.module, "frames": frames,
                            "mechanism": {"type": x.mechanism.ty, "handled": x.mechanism.handled},
                        })
                    })
                    .collect();
                a.insert("fixwire.exceptions".into(), chain.into());
                if e.exceptions.iter().any(|x| !x.mechanism.handled) {
                    a.insert("fixwire.handled".into(), false.into());
                }
                if let Some(m) = &e.message {
                    record.insert("body".into(), any_value(&self.clean(m).into()));
                }
            }
        }
        record.insert(
            "attributes".into(),
            attributes(&self.scrub(a, &["fixwire.event_id"])).into(),
        );
        Value::Object(record)
    }
}

/// The instrumentation scope: the SDK.
pub(crate) fn scope() -> Value {
    json!({"name": crate::SDK_NAME, "version": crate::SDK_VERSION})
}

/// What an OTLP export carries.
#[derive(Clone, Copy)]
pub(crate) enum Export {
    Logs,
    Spans,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_values_as_otlp() {
        let mut m = Map::new();
        m.insert("b".into(), json!(1));
        m.insert("a".into(), json!({"x": [true, 1.5, "s"], "empty": ""}));
        m.insert("gone".into(), Value::Null);
        assert_eq!(
            Value::Array(attributes(&m)),
            json!([
                {"key": "a", "value": {"kvlistValue": {"values": [
                    {"key": "x", "value": {"arrayValue": {"values": [{"boolValue": true}, {"doubleValue": 1.5}, {"stringValue": "s"}]}}}
                ]}}},
                {"key": "b", "value": {"intValue": "1"}},
            ])
        );
    }
}
