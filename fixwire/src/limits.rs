//! The bounds on what is sent, the same in every Fixwire SDK
//! (`sdks/PROTOCOL.md` §13): strings cut on a character boundary, and the
//! values the app gives bounded in depth, breadth and size.

use std::borrow::Cow;

use serde_json::{Map, Value};

/// The bytes past a string's cut that redaction still reads, so a secret the
/// cut goes through (a private key, a JWT) is found and masked whole.
pub(crate) const REDACT_AHEAD: usize = 16 * 1024;

/// A value's containers are sent this many levels deep, with this many items
/// each, and this many in all.
const MAX_DEPTH: usize = 10;
const MAX_BREADTH: usize = 100;
const MAX_OBJECTS: usize = 10_000;

/// The first `max` bytes of `s` at most, cut on a character boundary.
pub(crate) fn head(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// `s` in at most `limit` bytes (3 or more): when it is longer, or `more`
/// says it was cut already, it is cut on a character boundary and ends in
/// `...`, within the limit.
pub(crate) fn cut(s: &str, limit: usize, more: bool) -> Cow<'_, str> {
    if s.len() <= limit && !more {
        return Cow::Borrowed(s);
    }
    Cow::Owned(format!("{}...", head(s, limit.saturating_sub(3))))
}

/// Every string in `v`, keys included, cut to `limit` bytes: what is sent
/// when redaction is off.
pub(crate) fn cut_strings(v: Value, limit: usize) -> Value {
    match v {
        Value::String(s) if s.len() > limit => Value::String(cut(&s, limit, false).into_owned()),
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|x| cut_strings(x, limit)).collect())
        }
        Value::Object(m) => {
            let mut out = Map::with_capacity(m.len());
            for (k, x) in m {
                // Keys that are alike once cut: the first one stays.
                out.entry(cut(&k, limit, false).into_owned())
                    .or_insert_with(|| cut_strings(x, limit));
            }
            Value::Object(out)
        }
        other => other,
    }
}

/// A value the app gave, as it is sent: containers at most 10 levels deep
/// (one deeper is `"[Object]"` or `"[Array]"`), with their first 100 items,
/// and at most 10,000 of them walked (the rest are `"[Object]"` or
/// `"[Array]"` too).
pub(crate) fn bounded(v: &Value) -> Value {
    let mut left = MAX_OBJECTS;
    bound(v, 0, &mut left)
}

/// A map the app gave (extras, contexts, a breadcrumb's data), bounded as a
/// value.
pub(crate) fn bounded_map(m: &Map<String, Value>) -> Map<String, Value> {
    let mut left = MAX_OBJECTS - 1;
    bound_map(m, 0, &mut left)
}

fn bound(v: &Value, depth: usize, left: &mut usize) -> Value {
    match v {
        Value::Array(_) | Value::Object(_) if depth >= MAX_DEPTH || *left == 0 => {
            Value::from(if v.is_array() { "[Array]" } else { "[Object]" })
        }
        Value::Array(items) => {
            *left -= 1;
            Value::Array(
                items
                    .iter()
                    .take(MAX_BREADTH)
                    .map(|x| bound(x, depth + 1, left))
                    .collect(),
            )
        }
        Value::Object(m) => {
            *left -= 1;
            Value::Object(bound_map(m, depth, left))
        }
        other => other.clone(),
    }
}

fn bound_map(m: &Map<String, Value>, depth: usize, left: &mut usize) -> Map<String, Value> {
    m.iter()
        .take(MAX_BREADTH)
        .map(|(k, x)| (k.clone(), bound(x, depth + 1, left)))
        .collect()
}

/// A number as it is sent: `NaN` and the infinities, which JSON has no
/// numbers for, as the strings `"NaN"`, `"Infinity"` and `"-Infinity"`.
#[cfg_attr(not(feature = "tracing"), allow(dead_code))]
pub(crate) fn float(f: f64) -> Value {
    match f {
        f if f.is_nan() => "NaN".into(),
        f64::INFINITY => "Infinity".into(),
        f64::NEG_INFINITY => "-Infinity".into(),
        f => f.into(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn strings_are_cut_on_a_character_boundary_within_the_limit() {
        let ascii = "a".repeat(1024);
        assert_eq!(cut(&ascii, 1024, false), ascii.as_str(), "1024 bytes stay");
        let over = "a".repeat(1025);
        let got = cut(&over, 1024, false);
        assert_eq!((got.len(), &got[1018..]), (1024, "aaa..."));
        // 1025 bytes: "é" (2 bytes) runs across byte 1021.
        let multi = format!("{}{}", "a".repeat(1020), "é".repeat(2)) + "a";
        assert_eq!(multi.len(), 1025);
        let got = cut(&multi, 1024, false);
        assert_eq!(got, format!("{}...", "a".repeat(1020)));
        assert!(got.len() <= 1024);
        let emoji = "😀".repeat(300); // 1200 bytes, 4 each
        let got = cut(&emoji, 1024, false);
        assert_eq!(got, format!("{}...", "😀".repeat(255)));
        assert_eq!(cut("short", 1024, true), "short...", "cut before");
        assert_eq!(head("é", 1), "");
    }

    #[test]
    fn values_are_bounded_in_depth_breadth_and_size() {
        let nest = |depth: usize| (0..depth).fold(json!("x"), |v, _| json!({"a": v}));
        let mut v = &bounded(&nest(12));
        for _ in 0..10 {
            v = &v["a"];
        }
        assert_eq!(v, "[Object]", "ten levels, then a marker");
        let mut v = &bounded(&nest(10));
        for _ in 0..10 {
            v = &v["a"];
        }
        assert_eq!(v, "x", "ten levels are kept");
        let deep_list = (0..11).fold(json!(1), |v, _| json!([v]));
        assert_eq!(
            bounded(&deep_list).to_string(),
            format!("{}\"[Array]\"{}", "[".repeat(10), "]".repeat(10))
        );

        let wide = Value::Array((0..150).map(Value::from).collect());
        assert_eq!(bounded(&wide).as_array().unwrap().len(), 100);
        let wide: Map<String, Value> = (0..150).map(|i| (i.to_string(), json!(i))).collect();
        assert_eq!(bounded_map(&wide).len(), 100);

        // 1 + 100 + 100 × 100 lists: past 10,000 they are markers (the last
        // list of the second level, so its 100 aren't walked).
        fn count(v: &Value) -> (usize, usize) {
            match v {
                Value::Array(items) => items
                    .iter()
                    .map(count)
                    .fold((1, 0), |(c, m), (c2, m2)| (c + c2, m + m2)),
                Value::String(s) if s == "[Array]" => (0, 1),
                _ => (0, 0),
            }
        }
        let big = Value::Array(vec![Value::Array(vec![json!([1]); 100]); 100]);
        assert_eq!(count(&big), (10_101, 0));
        assert_eq!(count(&bounded(&big)), (10_000, 1));
    }

    #[test]
    fn infinities_and_nan_are_strings() {
        assert_eq!(float(f64::NAN), "NaN");
        assert_eq!(float(f64::INFINITY), "Infinity");
        assert_eq!(float(f64::NEG_INFINITY), "-Infinity");
        assert_eq!(float(1.5), json!(1.5));
    }

    #[test]
    fn strings_and_keys_are_cut_without_redaction() {
        let long = "k".repeat(2000);
        let v = cut_strings(json!({ long.clone(): [long.clone()], "b": 1 }), 1024);
        let (k, x) = v.as_object().unwrap().iter().next().unwrap();
        assert_eq!((k.len(), x[0].as_str().unwrap().len()), (1024, 1024));
    }
}
