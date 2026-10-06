//! The text being searched, and the server's hand-written scanners for the
//! detectors whose regular expressions would otherwise try every position of
//! digit-heavy text. They read bytes as the server's do: every byte they test
//! for is ASCII, so a multi-byte character never matches and every span
//! starts and ends on a character boundary.

use std::borrow::Cow;
use std::cell::OnceCell;

use super::validators;

/// A `[start, end)` byte range.
pub(super) type Span = (usize, usize);

/// One string being searched, with what several detectors share.
pub(super) struct Text<'a> {
    string: &'a str,
    lower: OnceCell<Cow<'a, str>>,
    numbers: OnceCell<Vec<NumberRun>>,
}

impl<'a> Text<'a> {
    pub(super) fn new(string: &'a str) -> Self {
        Text {
            string,
            lower: OnceCell::new(),
            numbers: OnceCell::new(),
        }
    }

    pub(super) fn string(&self) -> &'a str {
        self.string
    }

    /// The text in lower case for the prefilters, which are ASCII. On the
    /// server only A-Z, U+0130 ("i") and the Kelvin sign ("k") lower-case to
    /// ASCII, so only those are mapped; every other character stays, as its
    /// lower case could not match an ASCII literal either.
    pub(super) fn lower(&self) -> &str {
        self.lower.get_or_init(|| {
            let s = self.string;
            // 0xC4 and 0xE2 lead U+0130 and U+212A in UTF-8.
            let mapped = |b: u8| b.is_ascii_uppercase() || b == 0xC4 || b == 0xE2;
            if !s.bytes().any(mapped) {
                return Cow::Borrowed(s);
            }
            let lower = s.to_ascii_lowercase();
            if lower.contains(['\u{130}', '\u{212A}']) {
                Cow::Owned(lower.replace('\u{130}', "i").replace('\u{212A}', "k"))
            } else {
                Cow::Owned(lower)
            }
        })
    }

    fn numbers(&self) -> &[NumberRun] {
        self.numbers
            .get_or_init(|| number_runs(self.string.as_bytes()))
    }
}

fn is_word(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphanumeric()
}

/// A run of digits, optionally split by single spaces or dashes (one kind per
/// run), that stands alone as a word.
struct NumberRun {
    start: usize,
    end: usize,
    digits: usize,
    /// The separator, 0 when unbroken.
    sep: u8,
    /// The digit groups were 3, 2 and 4 long.
    ssn_groups: bool,
}

/// The server's numberSpans: a run starts at a digit after no word character
/// and is kept only when no word character follows it.
fn number_runs(s: &[u8]) -> Vec<NumberRun> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        if !s[i].is_ascii_digit() || (i > 0 && is_word(s[i - 1])) {
            i += 1;
            continue;
        }
        let mut digits = 0;
        let mut sep = 0u8;
        let mut groups = [0usize; 3];
        let mut group_count = 0;
        let mut group = 0;
        let mut j = i;
        while j < s.len() {
            let c = s[j];
            if c.is_ascii_digit() {
                digits += 1;
                group += 1;
                j += 1;
                continue;
            }
            if (c == b' ' || c == b'-')
                && j + 1 < s.len()
                && s[j + 1].is_ascii_digit()
                && (sep == 0 || sep == c)
            {
                sep = c;
                if group_count < 3 {
                    groups[group_count] = group;
                }
                group_count += 1;
                group = 0;
                j += 1;
                continue;
            }
            break;
        }
        if group_count < 3 {
            groups[group_count] = group;
        }
        group_count += 1;
        if j == s.len() || !is_word(s[j]) {
            out.push(NumberRun {
                start: i,
                end: j,
                digits,
                sep,
                ssn_groups: group_count == 3 && groups == [3, 2, 4],
            });
        }
        i = j + 1;
    }
    out
}

/// The number runs that `keep` accepts, validated on their text.
fn number_spans(
    text: &Text<'_>,
    keep: fn(&NumberRun) -> bool,
    validate: fn(&str) -> bool,
) -> Vec<Span> {
    text.numbers()
        .iter()
        .filter(|n| keep(n) && validate(&text.string()[n.start..n.end]))
        .map(|n| (n.start, n.end))
        .collect()
}

pub(super) fn card_spans(text: &Text<'_>) -> Vec<Span> {
    number_spans(text, |n| (13..=19).contains(&n.digits), validators::card)
}

pub(super) fn ssn_spans(text: &Text<'_>) -> Vec<Span> {
    number_spans(text, |n| n.sep == b'-' && n.ssn_groups, validators::ssn)
}

pub(super) fn tckn_spans(text: &Text<'_>) -> Vec<Span> {
    number_spans(text, |n| n.sep == 0 && n.digits == 11, validators::tckn)
}

/// The server's emailSpans: grows outwards from each "@" over the characters
/// an address may hold, and keeps it if the domain ends in a dotted,
/// alphabetic TLD. Spans may overlap ("a@b.co@c.de"); the first wins.
pub(super) fn email_spans(text: &Text<'_>) -> Vec<Span> {
    let s = text.string().as_bytes();
    let local = |c: u8| is_word(c) || matches!(c, b'.' | b'%' | b'+' | b'-');
    let domain = |c: u8| (is_word(c) && c != b'_') || c == b'.' || c == b'-';
    let mut out = Vec::new();
    let mut at = memchr(b'@', s, 0);
    while let Some(i) = at {
        let (mut start, mut end) = (i, i + 1);
        while start > 0 && local(s[start - 1]) {
            start -= 1;
        }
        while end < s.len() && domain(s[end]) {
            end += 1;
        }
        while end > i + 1 && (s[end - 1] == b'.' || s[end - 1] == b'-') {
            end -= 1;
        }
        let dom = &s[i + 1..end];
        if let Some(dot) = dom.iter().rposition(|&c| c == b'.')
            && dot > 0
            && start < i
        {
            let tld = &dom[dot + 1..];
            let ok = (2..=24).contains(&tld.len()) && tld.iter().all(u8::is_ascii_alphabetic);
            while start < i && (s[start] == b'.' || s[start] == b'-') {
                start += 1;
            }
            if ok && start < i {
                out.push((start, end));
            }
        }
        at = memchr(b'@', s, i + 1);
    }
    out
}

fn memchr(needle: u8, s: &[u8], from: usize) -> Option<usize> {
    s[from..]
        .iter()
        .position(|&c| c == needle)
        .map(|p| from + p)
}

/// The server's mayHoldIBAN: two capitals and two digits start a word.
pub(super) fn may_hold_iban(s: &str) -> bool {
    let s = s.as_bytes();
    s.windows(4).enumerate().any(|(i, w)| {
        w[0].is_ascii_uppercase()
            && w[1].is_ascii_uppercase()
            && w[2].is_ascii_digit()
            && w[3].is_ascii_digit()
            && (i == 0 || !is_word(s[i - 1]))
    })
}
