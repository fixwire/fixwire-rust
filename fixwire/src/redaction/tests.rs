//! The shared corpus (a copy of `pkg/redact/testdata/vectors.json` in
//! fixwire/fixwire, kept identical) and cases beyond
//! it. Every expected value is what the Fixwire server's redaction answers
//! for the same input. Secret-shaped fixtures are split, so no secret scanner
//! takes the tests for a leak.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::{Value, json};

use super::detectors::REGISTRY;
use super::{DEFAULT_DETECTORS, DEFAULT_SENSITIVE_KEYS, Redactor, go_lower};

const AWS: &str = concat!("AKIA", "IOSFODNN7EXAMPLE");
const JWT: &str = concat!(
    "eyJ",
    "hbGciOiJIUzI1NiJ9.",
    "eyJ",
    "zdWIiOiIxMjM0NTY3ODkwIn0.",
    "dozjgNryP4J3jVmNHl0w5N"
);

fn redactor() -> &'static Redactor {
    Redactor::default_shared()
}

#[track_caller]
fn assert_mask(input: &str, masked: &str, findings: &[&str]) {
    let (got, found) = redactor().mask(input);
    assert_eq!(
        (got.as_ref(), found.as_slice()),
        (masked, findings),
        "input {input:?}"
    );
}

#[track_caller]
fn assert_walk(r: &Redactor, input: &str, masked: &str, count: usize) {
    let input: Value = serde_json::from_str(input).unwrap();
    let masked: Value = serde_json::from_str(masked).unwrap();
    assert_eq!(r.walk(&input), (masked, count), "input {input}");
}

/// The crate's copy of the corpus, kept identical to the server's.
fn vectors_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/vectors.json")
}

/// Replaces each {{name}} with the joined parts of that fixture, in strings
/// and keys.
fn expand(value: &Value, fixtures: &[(String, String)]) -> Value {
    let text = |s: &str| {
        fixtures.iter().fold(s.to_owned(), |s, (name, whole)| {
            s.replace(&format!("{{{{{name}}}}}"), whole)
        })
    };
    match value {
        Value::String(s) => Value::String(text(s)),
        Value::Array(items) => Value::Array(items.iter().map(|x| expand(x, fixtures)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, x)| (text(k), expand(x, fixtures)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[test]
fn shared_vectors() {
    let corpus: Value =
        serde_json::from_str(&std::fs::read_to_string(vectors_path()).unwrap()).unwrap();
    assert_eq!(corpus["detectors"], json!(DEFAULT_DETECTORS));
    assert_eq!(corpus["sensitive_keys"], json!(DEFAULT_SENSITIVE_KEYS));
    let fixtures: Vec<(String, String)> = corpus["fixtures"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, parts)| {
            let parts = parts.as_array().unwrap().iter();
            (name.clone(), parts.map(|p| p.as_str().unwrap()).collect())
        })
        .collect();
    let r = redactor();
    let strings = corpus["strings"].as_array().unwrap();
    for case in strings {
        let input = expand(&case["input"], &fixtures);
        let (masked, found) = r.mask(input.as_str().unwrap());
        assert_eq!(masked, case["masked"].as_str().unwrap(), "{}", case["name"]);
        assert_eq!(json!(found), case["findings"], "{}", case["name"]);
    }
    let documents = corpus["documents"].as_array().unwrap();
    for case in documents {
        let input = expand(&case["input"], &fixtures);
        let (masked, count) = r.walk(&input);
        assert_eq!(masked, case["masked"], "{}", case["name"]);
        assert_eq!(json!(count), case["count"], "{}", case["name"]);
        if count == 0 {
            assert_eq!(masked, input, "{}", case["name"]);
        }
    }
    assert!(strings.len() >= 40 && documents.len() >= 9);
}

#[test]
fn detectors_in_the_servers_order_with_ipv4_off() {
    let names: Vec<&str> = REGISTRY.iter().map(|d| d.name).collect();
    assert_eq!(names[..21], DEFAULT_DETECTORS);
    assert_eq!(names[21], "ipv4");
    // Every pattern compiles.
    for d in &REGISTRY {
        d.spans(&super::scanners::Text::new("x"));
    }
    assert_mask("client 203.0.113.9", "client 203.0.113.9", &[]);
}

#[test]
fn ipv4_on_request() {
    let only = Redactor::with_detectors(&["ipv4"], None).unwrap();
    let cases = [
        (
            "client 203.0.113.9 and 10.0.0.5",
            "client [REDACTED:ipv4] and [REDACTED:ipv4]",
            &["ipv4"; 2][..],
        ),
        (
            "256.1.1.1 1.2.3.4.5 01.2.3.4 a1.2.3.4 1.2.3.4a 1.2.3.4\u{e9} \u{e9}1.2.3.4 1.2.3.\u{664}",
            "256.1.1.1 [REDACTED:ipv4].5 01.2.3.4 a1.2.3.4 1.2.3.4a [REDACTED:ipv4]\u{e9} \u{e9}[REDACTED:ipv4] 1.2.3.\u{664}",
            &["ipv4"; 3][..],
        ),
    ];
    for (input, masked, findings) in cases {
        let (got, found) = only.mask(input);
        assert_eq!((got.as_ref(), found.as_slice()), (masked, findings));
    }
    let mut all = DEFAULT_DETECTORS.to_vec();
    all.push("ipv4");
    let r = Redactor::with_detectors(&all, None).unwrap();
    // "5 4111…" is one run of 17 digits, no card.
    let (got, found) = r.mask("ada@example.com 10.0.0.5 4111111111111111");
    assert_eq!(got, "[REDACTED:email] [REDACTED:ipv4] 4111111111111111");
    assert_eq!(found, ["email", "ipv4"]);
    // The earlier detector wins an overlap.
    let first = |names: &[&str]| {
        Redactor::with_detectors(names, None)
            .unwrap()
            .mask("1.2.3.4@x.io")
            .1
    };
    assert_eq!(first(&["ipv4", "email"]), ["ipv4"]);
    assert_eq!(first(&["email", "ipv4"]), ["email"]);
    assert_eq!(
        Redactor::with_detectors(&["nope"], None).err().unwrap(),
        "redact: unknown detector \"nope\""
    );
}

#[test]
fn default_is_shared() {
    fn shareable<T: Send + Sync>(_: &T) {}
    shareable(redactor());
    assert!(format!("{:?}", redactor()).starts_with("Redactor { detectors: [\"private_key\""));
    assert!(std::ptr::eq(
        Redactor::default_shared(),
        Redactor::default_shared()
    ));
    let (masked, found) = redactor().mask("nothing to see");
    assert!(matches!(masked, Cow::Borrowed("nothing to see")));
    assert!(found.is_empty());
    assert_mask("", "", &[]);
}

#[test]
fn case_folding_as_the_server() {
    // The Kelvin sign folds to "k" in the case-insensitive patterns (letter
    // classes too) and lower-cases to "k" for their prefilters.
    assert_mask(
        "TO\u{212a}EN=abcdefg",
        "TO\u{212a}EN=[REDACTED:secret_assignment]",
        &["secret_assignment"],
    );
    assert_mask(
        &format!("Account\u{212a}ey={}==", "A".repeat(86)),
        "Account\u{212a}ey=[REDACTED:azure_storage_key]",
        &["azure_storage_key"],
    );
    assert_mask(
        &format!("AccountKey={}==", "\u{212a}".repeat(86)),
        "AccountKey=[REDACTED:azure_storage_key]",
        &["azure_storage_key"],
    );
    assert_mask(
        &format!("accountkey={}A==", "\u{17f}".repeat(85)),
        "accountkey=[REDACTED:azure_storage_key]",
        &["azure_storage_key"],
    );
    // The long s folds to "s" but stays itself in lower case, so the
    // prefilter needs another literal.
    assert_mask(
        "pa\u{17f}\u{17f}word=abcdefgh",
        "pa\u{17f}\u{17f}word=abcdefgh",
        &[],
    );
    assert_mask(
        "pa\u{17f}\u{17f}word=abcdefgh pwd",
        "pa\u{17f}\u{17f}word=[REDACTED:secret_assignment] pwd",
        &["secret_assignment"],
    );
    assert_mask(
        "ba\u{17f}ic dXNlcjpwYXNz basic",
        "ba\u{17f}ic [REDACTED:http_auth] basic",
        &["http_auth"],
    );
    // The long s is no ASCII word character: the word boundary before it
    // needs a word character on the left.
    assert_mask(
        "x\u{17f}ecret=abcdefgh token",
        "x\u{17f}ecret=[REDACTED:secret_assignment] token",
        &["secret_assignment"],
    );
    assert_mask(
        "\u{17f}ecret=abcdefgh token",
        "\u{17f}ecret=abcdefgh token",
        &[],
    );
    // U+0130 lower-cases to "i" for the prefilter but folds to nothing.
    assert_mask(
        "BAS\u{130}C abcdefghijkl1",
        "BAS\u{130}C abcdefghijkl1",
        &[],
    );
    assert_mask(
        "ba\u{17f}ic dXNlcjpwYXNz bas\u{130}c",
        "ba\u{17f}ic [REDACTED:http_auth] bas\u{130}c",
        &["http_auth"],
    );
    // No folding beyond simple folding: not the sharp s for "ss", not
    // full-width letters.
    assert_mask(
        "pa\u{df}word=abcdefgh pass",
        "pa\u{df}word=abcdefgh pass",
        &[],
    );
    assert_mask(
        "PA\u{1e9e}WORD=abcdefgh pass",
        "PA\u{1e9e}WORD=abcdefgh pass",
        &[],
    );
    assert_mask(
        "\u{ff34}\u{ff2f}\u{ff2b}\u{ff25}\u{ff2e}=abcdefgh token",
        "\u{ff34}\u{ff2f}\u{ff2b}\u{ff25}\u{ff2e}=abcdefgh token",
        &[],
    );
    // The Kelvin sign is no ASCII capital for the credential check.
    assert_mask(
        "Bearer abcdefghij\u{212a}lm bearer",
        "Bearer abcdefghij\u{212a}lm bearer",
        &[],
    );
}

#[test]
fn repetitions_count_code_points() {
    assert_mask("password=ab\u{1f600}cd", "password=ab\u{1f600}cd", &[]);
    assert_mask(
        "password=ab\u{1f600}cde",
        "password=[REDACTED:secret_assignment]",
        &["secret_assignment"],
    );
}

#[test]
fn classes_and_boundaries_are_ascii() {
    // Other scripts' digits are no digits.
    assert_mask(
        &format!(
            "1{} 4111111111111111 \u{661}\u{662}\u{663}",
            "\u{663}".repeat(11)
        ),
        &format!(
            "1{} [REDACTED:credit_card] \u{661}\u{662}\u{663}",
            "\u{663}".repeat(11)
        ),
        &["credit_card"],
    );
    assert_mask(
        &format!("sk-ant-api\u{660}\u{663}-{}", "a".repeat(80)),
        &format!("sk-ant-api\u{660}\u{663}-{}", "a".repeat(80)),
        &[],
    );
    assert_mask(
        "call +\u{664} 555 123 4567 or +4 555 123 4567",
        "call +\u{664} 555 123 4567 or [REDACTED:phone]",
        &["phone"],
    );
    assert_mask(
        "GB\u{668}\u{662} WEST 1234 5698 7654 32 or GB82 WEST 1234 5698 7654 32",
        "GB\u{668}\u{662} WEST 1234 5698 7654 32 or [REDACTED:iban]",
        &["iban"],
    );
    // Whitespace is [\t\n\f\r ]: no vertical tab, no Unicode spaces (which
    // a negated class then takes).
    assert_mask(
        "Bearer\u{b}abcdefghijkl1 Bearer abcdefghijkl1",
        "Bearer\u{b}abcdefghijkl1 Bearer [REDACTED:http_auth]",
        &["http_auth"],
    );
    assert_mask(
        "token\u{b}=abcdefgh token=\u{b}abcdefgh",
        "token\u{b}=abcdefgh token=[REDACTED:secret_assignment]",
        &["secret_assignment"],
    );
    assert_mask(
        "Bearer\u{a0}abcdefghijkl1 bearer\u{2003}abcdefghijkl1 basic\u{85}abcdefghijkl1",
        "Bearer\u{a0}abcdefghijkl1 bearer\u{2003}abcdefghijkl1 basic\u{85}abcdefghijkl1",
        &[],
    );
    assert_mask(
        "token=\u{a0}abcdefg token\u{3000}=abcdefgh",
        "token=[REDACTED:secret_assignment] token\u{3000}=abcdefgh",
        &["secret_assignment"],
    );
    assert_mask(
        "dial http://u:pa\u{a0}ss@h and http://u\u{2003}x:pass@h",
        "dial http://u:[REDACTED:url_credentials]@h and http://u\u{2003}x:[REDACTED:url_credentials]@h",
        &["url_credentials", "url_credentials"],
    );
    // Accented letters and other digits are no word characters.
    assert_mask(
        "\u{e9}ada@example.com\u{e9}",
        "\u{e9}[REDACTED:email]\u{e9}",
        &["email"],
    );
    assert_mask(
        "\u{e9}4111111111111111 \u{fc}4111111111111111",
        "\u{e9}[REDACTED:credit_card] \u{fc}[REDACTED:credit_card]",
        &["credit_card", "credit_card"],
    );
    assert_mask(
        &format!("x\u{e9}{AWS}"),
        "x\u{e9}[REDACTED:aws_access_key]",
        &["aws_access_key"],
    );
    assert_mask(
        &format!("{AWS}\u{e9} {AWS}\u{663}"),
        "[REDACTED:aws_access_key]\u{e9} [REDACTED:aws_access_key]\u{663}",
        &["aws_access_key", "aws_access_key"],
    );
    assert_mask(
        "\u{fc}@example.com caf\u{e9}@example.com",
        "\u{fc}@example.com caf\u{e9}@example.com",
        &[],
    );
}

#[test]
fn scanners_and_overlaps_as_the_server() {
    assert_mask(&format!("-{JWT}"), "-[REDACTED:jwt]", &["jwt"]);
    // Overlapping addresses: the first wins.
    assert_mask("a@b.co@c.de", "[REDACTED:email]@c.de", &["email"]);
    assert_mask(
        "dial 1http://u:p@h and x_http://u:p@h and a+b://u:p@h",
        "dial 1http://u:p@h and x_http://u:p@h and a+b://u:[REDACTED:url_credentials]@h",
        &["url_credentials"],
    );
    assert_mask(
        "token=[Filtered] secret=[REDACTED:email] password=hunter22",
        "token=[Filtered] secret=[REDACTED:email] password=[REDACTED:secret_assignment]",
        &["secret_assignment"],
    );
    assert_mask(
        // Split, so no secret scanner takes the test for a key.
        concat!(
            "-----BEGIN RSA PRIVATE",
            " KEY-----\nMIIE\n-----END DSA PRIVATE KEY----- -----END RSA PRIVATE KEY-----"
        ),
        "[REDACTED:private_key] -----END RSA PRIVATE KEY-----",
        &["private_key"],
    );
}

#[test]
fn many_findings_in_one_text() {
    let input = "ada@example.com ".repeat(6000);
    let start = Instant::now();
    let (masked, found) = redactor().mask(&input);
    assert!(start.elapsed().as_millis() < 500);
    assert_eq!(found.len(), 6000);
    assert_eq!(masked, "[REDACTED:email] ".repeat(6000));
}

#[test]
fn hostile_inputs_take_linear_time() {
    let begin = "-----BEGIN RSA PRIVATE KEY-----";
    // About 100 KB each.
    let inputs = [
        format!("{}://", "a.".repeat(50_000)),
        format!("{begin}\nMIIE\n").repeat(3000),
        format!("-----BEGIN {}", "A".repeat(100)).repeat(1000),
        format!("a://b:{}", ":".repeat(100_000)),
        "a://b:c".repeat(14_000),
        "1://".repeat(25_000),
        "1-".repeat(50_000),
        "1 ".repeat(50_000),
        format!("{}{}", "a.".repeat(50_000), "@".repeat(1000)),
        "a@".repeat(50_000),
        format!("xoxb-{}", "a".repeat(300)).repeat(300),
        "ghp_".repeat(25_000),
        format!("ghp_{}", "a".repeat(100_000)),
        "pwd: abc ".repeat(11_000),
        format!("password{}", " ".repeat(100_000)),
        "Bearer ".repeat(14_000),
        "AB12 ".repeat(20_000),
        "+1 2 3 ".repeat(14_000),
        "eyJaaaaaaaaaa.".repeat(7000),
        "-eyJ".repeat(25_000),
        format!("-eyJaaaaaaaa.eyJ{}", "-eyJ".repeat(25_000)),
        format!("secret={}", "x".repeat(100_000)),
        "\u{e9}\u{1f600}\u{17f}\u{212a}".repeat(10_000),
    ];
    for (i, s) in inputs.iter().enumerate() {
        let start = Instant::now();
        redactor().mask(s);
        // Generous for unoptimized builds; release builds take about 1 ms.
        assert!(start.elapsed().as_millis() < 1000, "input {i}");
    }
}

#[test]
fn sensitive_keys() {
    assert_walk(
        redactor(),
        r#"{"Auth": "b", "auth": "a", "author": "c", "X-CSRF-Token": "q", "max_tokens": 5,
            "input_tokens": "x", "token_count": "y", "usage_token": "z", "empty_token": "",
            "none_token": null, "db_password": "[Filtered]"}"#,
        r#"{"Auth": "[Filtered]", "X-CSRF-Token": "[Filtered]", "auth": "[Filtered]", "author": "c",
            "db_password": "[Filtered]", "empty_token": "", "input_tokens": "x", "max_tokens": 5,
            "none_token": null, "token_count": "y", "usage_token": "z"}"#,
        3,
    );
    // Keys lower-case as the server's: U+0130 to "i", the Kelvin sign to "k".
    assert_walk(
        redactor(),
        "{\"CREDENT\u{130}AL\": \"x\", \"pa\u{17f}\u{17f}word\": \"y\", \"TO\u{212a}EN\": \"z\"}",
        "{\"CREDENT\u{130}AL\": \"[Filtered]\", \"TO\u{212a}EN\": \"[Filtered]\", \"pa\u{17f}\u{17f}word\": \"y\"}",
        2,
    );
}

#[test]
fn custom_keys_replace_the_defaults() {
    let cases: [(&[&str], &str, &str, usize); 6] = [
        (
            &["X-Api_Key"],
            r#"{"x api key": "s", "apikey": "t", "password": "u", "auth": "v"}"#,
            r#"{"apikey": "t", "auth": "[Filtered]", "password": "u", "x api key": "[Filtered]"}"#,
            2,
        ),
        (
            &[],
            r#"{"password": "u", "auth": "v"}"#,
            r#"{"auth": "[Filtered]", "password": "u"}"#,
            1,
        ),
        // Configured keys are normalized like the keys they are compared with.
        (
            &["Pass\u{130}ON"],
            "{\"passion\": \"a\", \"PASS\u{130}ON_fruit\": \"b\", \"pass\": \"c\"}",
            "{\"PASS\u{130}ON_fruit\": \"[Filtered]\", \"pass\": \"c\", \"passion\": \"[Filtered]\"}",
            2,
        ),
        (
            &["\u{1c89}x"],
            "{\"\u{1c89}x\": \"a\", \"\u{1c8a}x\": \"b\"}",
            "{\"\u{1c89}x\": \"[Filtered]\", \"\u{1c8a}x\": \"[Filtered]\"}",
            2,
        ),
        (
            &[""],
            r#"{"anything": "a", "n": 1}"#,
            r#"{"anything": "[Filtered]", "n": "[Filtered]"}"#,
            2,
        ),
        (
            &["Token"],
            r#"{"max_tokens": "a", "token": "b"}"#,
            r#"{"max_tokens": "a", "token": "[Filtered]"}"#,
            1,
        ),
    ];
    for (keys, input, masked, count) in cases {
        let keys: Vec<String> = keys.iter().map(|&k| k.to_owned()).collect();
        assert_walk(&Redactor::new(Some(&keys)), input, masked, count);
    }
}

#[test]
fn keys_lower_case_with_the_servers_tables() {
    assert_eq!(go_lower('\u{130}'), 'i');
    assert_eq!(go_lower('\u{212a}'), 'k');
    assert_eq!(go_lower('\u{1c89}'), '\u{1c8a}');
    assert_eq!(go_lower('\u{a7ce}'), '\u{a7cf}');
    assert_eq!(go_lower('\u{16ea0}'), '\u{16ebb}');
    assert_eq!(go_lower('\u{16eb8}'), '\u{16ed3}');
    assert_eq!(go_lower('\u{3a3}'), '\u{3c3}');
    assert_eq!(go_lower('\u{df}'), '\u{df}');
}

#[test]
fn renamed_keys_are_numbered_in_byte_order() {
    // U+FFFF sorts before U+1F600 in UTF-8 (not in UTF-16).
    assert_walk(
        redactor(),
        "{\"http://u:[REDACTED:url_credentials]@h\": 3, \"http://u:\u{1f600}x@h\": 2, \"http://u:\u{ffff}x@h\": 1}",
        r#"{"http://u:[REDACTED:url_credentials]@h": "[Filtered]",
            "http://u:[REDACTED:url_credentials]@h (2)": 1,
            "http://u:[REDACTED:url_credentials]@h (3)": 2}"#,
        3,
    );
    assert_walk(
        redactor(),
        r#"{"b@example.com": 2, "a@example.com": 1, "[REDACTED:email]": 0}"#,
        r#"{"[REDACTED:email]": 0, "[REDACTED:email] (2)": 1, "[REDACTED:email] (3)": 2}"#,
        2,
    );
}

#[test]
fn typed_attributes_and_pairs() {
    assert_walk(
        redactor(),
        r#"{"password": {"type": "int", "value": 5}, "token": {"type": "string", "value": "[Filtered]"},
            "secret": {"type": "x", "value": null},
            "headers": [["Authorization", "Bearer abc"], ["Cookie", ""], ["Accept", "a@b.co"]],
            "pair": ["password", "[Filtered]"], "map": {"a": "password", "b": "x"}}"#,
        r#"{"headers": [["Authorization", "[Filtered]"], ["Cookie", ""], ["Accept", "[REDACTED:email]"]],
            "map": {"a": "password", "b": "x"}, "pair": ["password", "[Filtered]"],
            "password": {"type": "string", "value": "[Filtered]"}, "secret": "[Filtered]",
            "token": {"type": "string", "value": "[Filtered]"}}"#,
        5,
    );
    // A typed attribute gains a type; nothing else in it is walked.
    assert_walk(
        redactor(),
        r#"{"apikey": {"value": "k"}, "cvv": ["123"], "ssn": 123456789,
            "secret": {"type": "t", "value": "s", "note": "ada@example.com"}}"#,
        r#"{"apikey": {"type": "string", "value": "[Filtered]"}, "cvv": "[Filtered]",
            "secret": {"note": "ada@example.com", "type": "string", "value": "[Filtered]"},
            "ssn": "[Filtered]"}"#,
        4,
    );
    assert_walk(
        redactor(),
        r#"[1, 2.5, true, false, null, "ada@example.com", ["x", "y", "z"]]"#,
        r#"[1, 2.5, true, false, null, "[REDACTED:email]", ["x", "y", "z"]]"#,
        1,
    );
}

#[test]
fn key_order_is_kept_and_the_input_unchanged() {
    let input = json!({
        "z": 1,
        "ada@example.com": {"card": "4111111111111111", "n": [1, "b@example.com"]},
        "apikey": {"value": "k", "unit": "x"},
        "a": null,
    });
    let copy = input.clone();
    let (out, count) = redactor().walk(&input);
    assert_eq!(count, 4);
    assert_eq!(input, copy);
    let keys: Vec<&str> = out
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["z", "[REDACTED:email]", "apikey", "a"]);
    let typed: Vec<&str> = out["apikey"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(typed, ["value", "unit", "type"]);
    assert_eq!(
        out,
        json!({
            "z": 1,
            "[REDACTED:email]": {"card": "[REDACTED:credit_card]", "n": [1, "[REDACTED:email]"]},
            "apikey": {"value": "[Filtered]", "unit": "x", "type": "string"},
            "a": null,
        })
    );
}

#[test]
fn containers_deeper_than_the_limit_are_left_alone() {
    let nest = |depth: usize| (0..depth).fold(json!("ada@example.com"), |v, _| json!([v]));
    let deep = nest(600);
    assert_eq!(redactor().walk(&deep), (deep.clone(), 0));
    let (out, count) = redactor().walk(&nest(500));
    assert_eq!(count, 1);
    let mut inner = &out;
    while let Value::Array(items) = inner {
        inner = &items[0];
    }
    assert_eq!(inner, "[REDACTED:email]");
}
