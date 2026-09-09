//! Small strict JSON helpers. This module intentionally replaces a JSON dependency.

use std::collections::HashSet;

const MAX_DEPTH: usize = 96;

fn skip_ws(bytes: &[u8], mut index: usize) -> usize {
    while matches!(bytes.get(index), Some(b' ' | b'\n' | b'\r' | b'\t')) {
        index += 1;
    }
    index
}

fn hex4(bytes: &[u8]) -> Option<u16> {
    (bytes.len() == 4).then_some(())?;
    bytes.iter().try_fold(0_u16, |value, byte| {
        Some(value.checked_mul(16)? + (*byte as char).to_digit(16)? as u16)
    })
}

fn parse_string_at(input: &str, start: usize) -> Option<(String, usize)> {
    let bytes = input.as_bytes();
    (bytes.get(start) == Some(&b'"')).then_some(())?;
    let (mut output, mut index) = (String::new(), start + 1);
    while index < bytes.len() {
        match bytes[index] {
            b'"' => return Some((output, index + 1)),
            b'\\' => {
                index += 1;
                match *bytes.get(index)? {
                    b'"' => output.push('"'),
                    b'\\' => output.push('\\'),
                    b'/' => output.push('/'),
                    b'b' => output.push('\u{0008}'),
                    b'f' => output.push('\u{000c}'),
                    b'n' => output.push('\n'),
                    b'r' => output.push('\r'),
                    b't' => output.push('\t'),
                    b'u' => {
                        let first = hex4(bytes.get(index + 1..index + 5)?)?;
                        index += 4;
                        let codepoint = if (0xd800..=0xdbff).contains(&first) {
                            if bytes.get(index + 1) != Some(&b'\\')
                                || bytes.get(index + 2) != Some(&b'u')
                            {
                                return None;
                            }
                            let second = hex4(bytes.get(index + 3..index + 7)?)?;
                            if !(0xdc00..=0xdfff).contains(&second) {
                                return None;
                            }
                            index += 6;
                            0x10000 + (((first as u32) - 0xd800) << 10) + ((second as u32) - 0xdc00)
                        } else if (0xdc00..=0xdfff).contains(&first) {
                            return None;
                        } else {
                            first as u32
                        };
                        output.push(char::from_u32(codepoint)?);
                    }
                    _ => return None,
                }
                index += 1;
            }
            0x00..=0x1f => return None,
            _ => {
                let ch = input.get(index..)?.chars().next()?;
                output.push(ch);
                index += ch.len_utf8();
            }
        }
    }
    None
}

fn number_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start;
    if bytes.get(index) == Some(&b'-') {
        index += 1;
    }
    match bytes.get(index)? {
        b'0' => index += 1,
        b'1'..=b'9' => {
            index += 1;
            while matches!(bytes.get(index), Some(b'0'..=b'9')) {
                index += 1;
            }
        }
        _ => return None,
    }
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        if !matches!(bytes.get(index), Some(b'0'..=b'9')) {
            return None;
        }
        while matches!(bytes.get(index), Some(b'0'..=b'9')) {
            index += 1;
        }
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        if !matches!(bytes.get(index), Some(b'0'..=b'9')) {
            return None;
        }
        while matches!(bytes.get(index), Some(b'0'..=b'9')) {
            index += 1;
        }
    }
    Some(index)
}

fn value_end_inner(input: &str, start: usize, depth: usize) -> Option<usize> {
    if depth > MAX_DEPTH {
        return None;
    }
    let bytes = input.as_bytes();
    let start = skip_ws(bytes, start);
    match *bytes.get(start)? {
        b'"' => parse_string_at(input, start).map(|(_, end)| end),
        b'{' => {
            let mut index = skip_ws(bytes, start + 1);
            let mut seen = HashSet::new();
            if bytes.get(index) == Some(&b'}') {
                return Some(index + 1);
            }
            loop {
                let (key, end) = parse_string_at(input, index)?;
                if !seen.insert(key) {
                    return None;
                }
                index = skip_ws(bytes, end);
                if bytes.get(index) != Some(&b':') {
                    return None;
                }
                index = skip_ws(bytes, value_end_inner(input, index + 1, depth + 1)?);
                match bytes.get(index) {
                    Some(b',') => index = skip_ws(bytes, index + 1),
                    Some(b'}') => return Some(index + 1),
                    _ => return None,
                }
            }
        }
        b'[' => {
            let mut index = skip_ws(bytes, start + 1);
            if bytes.get(index) == Some(&b']') {
                return Some(index + 1);
            }
            loop {
                index = skip_ws(bytes, value_end_inner(input, index, depth + 1)?);
                match bytes.get(index) {
                    Some(b',') => index = skip_ws(bytes, index + 1),
                    Some(b']') => return Some(index + 1),
                    _ => return None,
                }
            }
        }
        b't' if bytes.get(start..start + 4) == Some(b"true") => Some(start + 4),
        b'f' if bytes.get(start..start + 5) == Some(b"false") => Some(start + 5),
        b'n' if bytes.get(start..start + 4) == Some(b"null") => Some(start + 4),
        b'-' | b'0'..=b'9' => number_end(bytes, start),
        _ => None,
    }
}

fn value_end(input: &str, start: usize) -> Option<usize> {
    value_end_inner(input, start, 0)
}
pub fn validate(input: &str) -> bool {
    value_end(input, 0).is_some_and(|end| skip_ws(input.as_bytes(), end) == input.len())
}

pub fn escape(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 8);
    for ch in value.chars() {
        match ch {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{0008}' => output.push_str("\\b"),
            '\u{000c}' => output.push_str("\\f"),
            c if c <= '\u{001f}' => {
                use std::fmt::Write as _;
                let _ = write!(output, "\\u{:04x}", c as u32);
            }
            c => output.push(c),
        }
    }
    output
}
pub fn quote(value: &str) -> String {
    format!("\"{}\"", escape(value))
}
pub fn string(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let start = skip_ws(bytes, 0);
    let (decoded, end) = parse_string_at(value, start)?;
    (skip_ws(bytes, end) == bytes.len()).then_some(decoded)
}
pub fn object_entries(object: &str) -> Option<Vec<(String, &str)>> {
    let bytes = object.as_bytes();
    let mut index = skip_ws(bytes, 0);
    if bytes.get(index) != Some(&b'{') {
        return None;
    }
    index = skip_ws(bytes, index + 1);
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    if bytes.get(index) == Some(&b'}') {
        return (skip_ws(bytes, index + 1) == bytes.len()).then_some(entries);
    }
    loop {
        let (key, end) = parse_string_at(object, index)?;
        if !seen.insert(key.clone()) {
            return None;
        }
        index = skip_ws(bytes, end);
        if bytes.get(index) != Some(&b':') {
            return None;
        }
        let start = skip_ws(bytes, index + 1);
        let end = value_end(object, start)?;
        entries.push((key, object.get(start..end)?));
        index = skip_ws(bytes, end);
        match bytes.get(index) {
            Some(b',') => index = skip_ws(bytes, index + 1),
            Some(b'}') => return (skip_ws(bytes, index + 1) == bytes.len()).then_some(entries),
            _ => return None,
        }
    }
}
pub fn object_get<'a>(object: &'a str, wanted: &str) -> Option<&'a str> {
    object_entries(object)?
        .into_iter()
        .find_map(|(key, value)| (key == wanted).then_some(value))
}
pub fn array_values(array: &str) -> Option<Vec<&str>> {
    let bytes = array.as_bytes();
    let mut index = skip_ws(bytes, 0);
    if bytes.get(index) != Some(&b'[') {
        return None;
    }
    index = skip_ws(bytes, index + 1);
    let mut values = Vec::new();
    if bytes.get(index) == Some(&b']') {
        return (skip_ws(bytes, index + 1) == bytes.len()).then_some(values);
    }
    loop {
        let end = value_end(array, index)?;
        values.push(array.get(index..end)?);
        index = skip_ws(bytes, end);
        match bytes.get(index) {
            Some(b',') => index = skip_ws(bytes, index + 1),
            Some(b']') => return (skip_ws(bytes, index + 1) == bytes.len()).then_some(values),
            _ => return None,
        }
    }
}
pub fn boolean(value: &str) -> Option<bool> {
    match value.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

pub fn number(value: &str) -> bool {
    let bytes = value.as_bytes();
    let start = skip_ws(bytes, 0);
    let Some(end) = number_end(bytes, start) else {
        return false;
    };
    skip_ws(bytes, end) == bytes.len()
}

pub fn integer(value: &str) -> bool {
    let raw = value.trim();
    if !number(raw) {
        return false;
    }

    let unsigned = raw.strip_prefix('-').unwrap_or(raw);
    let (mantissa, exponent) = unsigned.find(['e', 'E']).map_or((unsigned, "0"), |index| {
        (&unsigned[..index], &unsigned[index + 1..])
    });
    let (whole, fraction) = mantissa
        .split_once('.')
        .map_or((mantissa, ""), |(whole, fraction)| (whole, fraction));
    let digits = format!("{whole}{fraction}");
    let zero = digits.bytes().all(|byte| byte == b'0');

    let (negative_exponent, exponent_digits) = match exponent.as_bytes().first() {
        Some(b'+') => (false, &exponent[1..]),
        Some(b'-') => (true, &exponent[1..]),
        _ => (false, exponent),
    };
    let exponent_value = match exponent_digits.parse::<i128>() {
        Ok(value) => {
            if negative_exponent {
                -value
            } else {
                value
            }
        }
        Err(_) => return if negative_exponent { zero } else { true },
    };

    let scale = exponent_value.saturating_sub(fraction.len() as i128);
    if scale >= 0 {
        return true;
    }

    let required_zeros = scale.unsigned_abs();
    if required_zeros > digits.len() as u128 {
        return zero;
    }
    digits
        .as_bytes()
        .iter()
        .rev()
        .take(required_zeros as usize)
        .all(|byte| *byte == b'0')
}
