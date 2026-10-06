//! Checks that reject look-alikes (Luhn, mod-97, check digits), so trace
//! ids, hashes and timestamps survive. Each takes the matched text.

use super::FILTERED;

/// Rejects values a scrubber already replaced.
pub(super) fn unmasked(v: &str) -> bool {
    !v.starts_with("[REDACTED") && v != FILTERED
}

/// Tells a token from a word after "basic": it has a digit, a base64 symbol,
/// or capitals past its first letter ("dXNlcjpwYXNz", but not
/// "Authentication"). The server reads the bytes after the first.
pub(super) fn credential_like(v: &str) -> bool {
    let b = v.as_bytes();
    if b.iter()
        .any(|c| c.is_ascii_digit() || matches!(c, b'+' | b'/' | b'='))
    {
        return true;
    }
    let rest = b.get(1..).unwrap_or_default();
    rest.iter().any(u8::is_ascii_uppercase) && rest.iter().any(u8::is_ascii_lowercase)
}

const CARD_PREFIXES: [&str; 18] = [
    "4", "51", "52", "53", "54", "55", "2221", "2720", "34", "37", "6011", "65", "35", "36", "38",
    "300", "305", "62",
];

/// Checks the length, a known issuer prefix and the Luhn sum.
pub(super) fn card(run: &str) -> bool {
    let digits: Vec<u8> = run.bytes().filter(u8::is_ascii_digit).collect();
    if !(13..=19).contains(&digits.len())
        || !CARD_PREFIXES
            .iter()
            .any(|p| digits.starts_with(p.as_bytes()))
    {
        return false;
    }
    let mut sum = 0u32;
    for (i, d) in digits.iter().rev().enumerate() {
        let mut n = u32::from(d - b'0');
        if i % 2 == 1 {
            n *= 2;
            if n > 9 {
                n -= 9;
            }
        }
        sum += n;
    }
    sum.is_multiple_of(10)
}

/// Checks the length (15 to 34) and the mod-97 checksum of the decimal
/// number the characters spell (A = 10 ... Z = 35), read from the fifth
/// character round to the fourth.
pub(super) fn iban(m: &str) -> bool {
    let s: Vec<u8> = m.bytes().filter(|&c| c != b' ').collect();
    if !(15..=34).contains(&s.len()) {
        return false;
    }
    let mut rem = 0u32;
    for &c in s[4..].iter().chain(&s[..4]) {
        rem = match c {
            b'0'..=b'9' => (rem * 10 + u32::from(c - b'0')) % 97,
            b'A'..=b'Z' => (rem * 100 + u32::from(c - b'A') + 10) % 97,
            _ => return false,
        };
    }
    rem == 1
}

/// Rejects numbers the US never issues (the run is "ddd-dd-dddd").
pub(super) fn ssn(run: &str) -> bool {
    let (area, group, serial) = (&run[0..3], &run[4..6], &run[7..11]);
    area != "000" && area != "666" && !area.starts_with('9') && group != "00" && serial != "0000"
}

/// Checks the Turkish identity number's two check digits.
pub(super) fn tckn(run: &str) -> bool {
    let b = run.as_bytes();
    if b.len() != 11 || b[0] == b'0' {
        return false;
    }
    let d: Vec<i32> = b.iter().map(|&c| i32::from(c) - i32::from(b'0')).collect();
    let odd = d[0] + d[2] + d[4] + d[6] + d[8];
    let even = d[1] + d[3] + d[5] + d[7];
    (odd * 7 - even).rem_euclid(10) == d[9] && d[..10].iter().sum::<i32>() % 10 == d[10]
}

/// Wants an international number of 8 to 15 digits.
pub(super) fn phone(m: &str) -> bool {
    (8..=15).contains(&m.bytes().filter(u8::is_ascii_digit).count())
}
