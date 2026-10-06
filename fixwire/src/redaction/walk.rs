//! The server's JSON walk: every string masked, the values of sensitive keys
//! filtered whole, keys masked too.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use super::{FILTERED, Redactor};

/// Containers nested deeper than this stay as they are (the server masks
/// them), so a hostile value cannot exhaust the stack.
const MAX_DEPTH: usize = 512;

impl Redactor {
    /// Masks every string in a JSON value and filters the values of
    /// sensitive keys, by the server's rules: a typed attribute
    /// (`{"type": …, "value": …}`) keeps its shape, a list of two holding a
    /// sensitive key and a value is a pair, and keys hold data too. Returns a
    /// new value, in the input's key order (with serde_json's `preserve_order`
    /// feature), and the number of values masked; the input is never changed.
    pub(crate) fn walk(&self, value: &Value) -> (Value, usize) {
        let mut count = 0;
        let out = self.walk_value(value, &mut count, 0);
        (out, count)
    }

    fn walk_value(&self, value: &Value, n: &mut usize, depth: usize) -> Value {
        match value {
            Value::String(s) => {
                let (masked, found) = self.mask(s);
                *n += found.len();
                Value::String(masked.into_owned())
            }
            Value::Array(items) if depth < MAX_DEPTH => self.walk_array(items, n, depth),
            Value::Object(map) if depth < MAX_DEPTH => self.walk_object(map, n, depth),
            other => other.clone(),
        }
    }

    /// Some maps are sent as `[key, value]` pairs (headers, tags).
    fn walk_array(&self, items: &[Value], n: &mut usize, depth: usize) -> Value {
        if let [Value::String(key), value] = items
            && !is_empty(value)
            && self.sensitive(key)
        {
            *n += 1;
            return Value::Array(vec![items[0].clone(), Value::from(FILTERED)]);
        }
        Value::Array(
            items
                .iter()
                .map(|item| self.walk_value(item, n, depth + 1))
                .collect(),
        )
    }

    fn walk_object(&self, map: &Map<String, Value>, n: &mut usize, depth: usize) -> Value {
        let mut out = Map::with_capacity(map.len());
        // Keys that mask to something else: (key, masked key, findings).
        let mut renamed = Vec::new();
        for (key, value) in map {
            let (masked, found) = self.mask(key);
            if !found.is_empty() {
                renamed.push((key.as_str(), masked.into_owned(), found.len()));
            }
            let value = if !is_empty(value) && self.sensitive(key) {
                filter(value, n)
            } else {
                self.walk_value(value, n, depth + 1)
            };
            out.insert(key.clone(), value);
        }
        if renamed.is_empty() {
            return Value::Object(out);
        }
        // Keys that mask alike are numbered in byte order of the original
        // keys, each taking the first name no key holds at its turn:
        // "[REDACTED:email] (2)". A renamed key keeps its place.
        renamed.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let mut taken: HashSet<String> = map.keys().cloned().collect();
        let mut names: HashMap<&str, String> = HashMap::with_capacity(renamed.len());
        for (key, masked, found) in renamed {
            let mut name = masked.clone();
            let mut i = 2;
            while taken.contains(&name) {
                name = format!("{masked} ({i})");
                i += 1;
            }
            taken.insert(name.clone());
            taken.remove(key);
            names.insert(key, name);
            *n += found;
        }
        Value::Object(
            out.into_iter()
                .map(|(key, value)| match names.remove(key.as_str()) {
                    Some(name) => (name, value),
                    None => (key, value),
                })
                .collect(),
        )
    }
}

/// The value of a sensitive key, filtered whole. A typed attribute keeps its
/// shape (its value filtered, its type "string"); nothing else in it is
/// walked.
fn filter(value: &Value, n: &mut usize) -> Value {
    if let Value::Object(attribute) = value
        && let Some(inner) = attribute.get("value")
        && !inner.is_null()
    {
        if inner.as_str() == Some(FILTERED) {
            return value.clone();
        }
        let mut attribute = attribute.clone();
        attribute.insert("value".to_owned(), Value::from(FILTERED));
        attribute.insert("type".to_owned(), Value::from("string"));
        *n += 1;
        return Value::Object(attribute);
    }
    if value.as_str() == Some(FILTERED) {
        return value.clone();
    }
    *n += 1;
    Value::from(FILTERED)
}

fn is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        _ => false,
    }
}
