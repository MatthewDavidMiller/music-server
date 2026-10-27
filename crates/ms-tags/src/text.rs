//! Text decoding and the small value parsers every tag format shares.

use crate::limits::MAX_TEXT;

/// Trims, drops control characters, cuts to [`MAX_TEXT`] bytes on a
/// character boundary, and turns an empty result into `None`.
pub(crate) fn clean(value: &str) -> Option<String> {
    let mut out = String::with_capacity(value.len().min(MAX_TEXT));
    for c in value.trim().chars() {
        if c.is_control() {
            continue;
        }
        if out.len() + c.len_utf8() > MAX_TEXT {
            break;
        }
        out.push(c);
    }
    let trimmed = out.trim_end();
    if trimmed.is_empty() { None } else { Some(trimmed.to_owned()) }
}

/// ISO-8859-1: every byte is the code point of the same value.
pub(crate) fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

pub(crate) fn utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// UTF-16 with an optional byte-order mark; without one, `big_endian`
/// decides.
pub(crate) fn utf16(bytes: &[u8], big_endian: bool) -> String {
    let (body, be) = match bytes {
        [0xfe, 0xff, rest @ ..] => (rest, true),
        [0xff, 0xfe, rest @ ..] => (rest, false),
        _ => (bytes, big_endian),
    };
    let units: Vec<u16> = body
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&p| if be { u16::from_be_bytes(p) } else { u16::from_le_bytes(p) })
        .collect();
    String::from_utf16_lossy(&units)
}

fn leading_number(s: &str) -> Option<u16> {
    let digits: String = s.trim().chars().take_while(char::is_ascii_digit).take(5).collect();
    digits.parse::<u32>().ok().and_then(|n| u16::try_from(n).ok()).filter(|&n| n > 0)
}

/// "3", "03", "3/12" (and "3 of 12"): the number and the total.
pub(crate) fn number_pair(s: &str) -> (Option<u16>, Option<u16>) {
    let s = s.trim();
    match s.split_once('/').or_else(|| s.split_once(" of ")) {
        Some((n, t)) => (leading_number(n), leading_number(t)),
        None => (leading_number(s), None),
    }
}

/// The year at the start of "1999", "1999-04-01", "1999-04-01T12:00".
pub(crate) fn year(s: &str) -> Option<u16> {
    let s = s.trim();
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    if digits.len() != 4 {
        return None;
    }
    digits.parse::<u16>().ok().filter(|&y| y > 0)
}

pub(crate) fn truthy(s: &str) -> bool {
    matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleaning() {
        assert_eq!(clean("  a\u{0}b\tc  "), Some("abc".to_owned()));
        assert_eq!(clean(" \u{1} "), None);
        let long = "é".repeat(MAX_TEXT);
        let cut = clean(&long).unwrap();
        assert!(cut.len() <= MAX_TEXT && cut.chars().all(|c| c == 'é'));
    }

    #[test]
    fn encodings() {
        assert_eq!(latin1(b"caf\xe9"), "café");
        assert_eq!(utf16(&[0xff, 0xfe, b'h', 0, b'i', 0], true), "hi");
        assert_eq!(utf16(&[0xfe, 0xff, 0, b'h', 0, b'i'], false), "hi");
        assert_eq!(utf16(&[0, b'h', 0, b'i', 0], true), "hi");
        assert_eq!(utf16(&[0xff, 0xfe, 0x00, 0xd8], false), "\u{fffd}");
    }

    #[test]
    fn numbers_and_years() {
        assert_eq!(number_pair("03"), (Some(3), None));
        assert_eq!(number_pair("3/12"), (Some(3), Some(12)));
        assert_eq!(number_pair("3 of 12"), (Some(3), Some(12)));
        assert_eq!(number_pair("/12"), (None, Some(12)));
        assert_eq!(number_pair("0"), (None, None));
        assert_eq!(number_pair("99999999"), (None, None));
        assert_eq!(year("1999-04-01"), Some(1999));
        assert_eq!(year("99"), None);
        assert_eq!(year("abcd"), None);
        assert!(truthy("Yes") && !truthy("0"));
    }
}
