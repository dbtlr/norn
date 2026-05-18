use std::collections::HashMap;

use serde_json::Value;
use vault_core::Document;

pub(crate) fn frontmatter_value_matches(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::String(actual), Value::String(expected)) => actual == expected,
        (Value::Bool(actual), Value::Bool(expected)) => actual == expected,
        (Value::Number(actual), Value::Number(expected)) => actual == expected,
        _ => false,
    }
}

pub(crate) fn frontmatter_type_matches(value: &Value, expected_type: &str) -> bool {
    match expected_type {
        "datetime" => value.as_str().is_some_and(is_datetime_string),
        "date" => value.as_str().is_some_and(is_date_string),
        "list_of_strings" => value
            .as_array()
            .is_some_and(|values| values.iter().all(|value| value.as_str().is_some())),
        "wikilink" => value.as_str().is_some_and(is_wikilink_string),
        "wikilink_or_list" => {
            value.as_str().is_some_and(is_wikilink_string)
                || value.as_array().is_some_and(|values| {
                    values
                        .iter()
                        .all(|value| value.as_str().is_some_and(is_wikilink_string))
                })
        }
        _ => false,
    }
}

pub(crate) fn is_datetime_string(value: &str) -> bool {
    let Some((date, time)) = value.split_once('T').or_else(|| value.split_once(' ')) else {
        return false;
    };

    is_date_string(date) && is_time_string(time)
}

pub(crate) fn is_date_string(value: &str) -> bool {
    if is_plain_date_string(value) {
        return true;
    }

    let Some((date, time)) = value.split_once('T').or_else(|| value.split_once(' ')) else {
        return false;
    };

    is_plain_date_string(date) && is_midnight_time_string(time)
}

fn is_plain_date_string(value: &str) -> bool {
    let mut parts = value.split('-');
    let (Some(year), Some(month), Some(day), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };

    if year.len() != 4
        || month.len() != 2
        || day.len() != 2
        || !year.chars().all(|char| char.is_ascii_digit())
    {
        return false;
    }

    let Ok(year) = year.parse::<u16>() else {
        return false;
    };
    let Ok(month) = month.parse::<u8>() else {
        return false;
    };
    let Ok(day) = day.parse::<u8>() else {
        return false;
    };

    (1..=days_in_month(year, month)).contains(&day)
}

pub(crate) fn is_time_string(value: &str) -> bool {
    parse_time(value).is_some()
}

fn is_midnight_time_string(value: &str) -> bool {
    parse_time(value).is_some_and(|time| time.hour == 0 && time.minute == 0 && time.second == 0)
}

fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: u16) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

struct ParsedTime {
    hour: u8,
    minute: u8,
    second: u8,
}

fn parse_time(value: &str) -> Option<ParsedTime> {
    let time = strip_timezone(value)?;
    let mut parts = time.split(':');
    let (Some(hour), Some(minute), maybe_second, None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };

    let hour = parse_two_digit_u8(hour, 23)?;
    let minute = parse_two_digit_u8(minute, 59)?;
    let second = maybe_second.map_or(Some(0), parse_second)?;

    Some(ParsedTime {
        hour,
        minute,
        second,
    })
}

fn strip_timezone(value: &str) -> Option<&str> {
    if let Some(time) = value.strip_suffix('Z') {
        return Some(time);
    }

    let Some(offset_start) = value.rfind(['+', '-']) else {
        return Some(value);
    };
    let (time, offset) = value.split_at(offset_start);
    validate_timezone_offset(offset).then_some(time)
}

fn validate_timezone_offset(offset: &str) -> bool {
    let Some(offset) = offset.strip_prefix(['+', '-']) else {
        return false;
    };
    let Some((hour, minute)) = offset.split_once(':') else {
        return false;
    };

    parse_two_digit_u8(hour, 23).is_some() && parse_two_digit_u8(minute, 59).is_some()
}

fn parse_second(value: &str) -> Option<u8> {
    let second = if let Some((second, fraction)) = value.split_once('.') {
        if fraction.is_empty() || !fraction.chars().all(|char| char.is_ascii_digit()) {
            return None;
        }
        second
    } else {
        value
    };
    parse_two_digit_u8(second, 59)
}

fn parse_two_digit_u8(value: &str, max: u8) -> Option<u8> {
    if value.len() != 2 || !value.chars().all(|char| char.is_ascii_digit()) {
        return None;
    }
    value.parse::<u8>().ok().filter(|value| *value <= max)
}

pub(crate) fn is_wikilink_string(value: &str) -> bool {
    value.starts_with("[[") && value.ends_with("]]") && value.len() > 4
}

pub(crate) fn document_has_frontmatter_field(document: &Document, field: &str) -> bool {
    document_frontmatter_field(document, field).is_some()
}

pub(crate) fn document_frontmatter_field<'a>(
    document: &'a Document,
    field: &str,
) -> Option<&'a Value> {
    document
        .frontmatter
        .as_ref()
        .and_then(|frontmatter| frontmatter.get(field))
        .filter(|value| !value.is_null())
}

pub(crate) fn frontmatter_predicates_match(
    document: &Document,
    predicates: &HashMap<String, Value>,
) -> bool {
    if predicates.is_empty() {
        return true;
    }

    let Some(frontmatter) = document.frontmatter.as_ref() else {
        return false;
    };

    predicates.iter().all(|(field, expected)| {
        frontmatter
            .get(field)
            .is_some_and(|actual| frontmatter_value_matches(actual, expected))
    })
}

#[cfg(test)]
mod tests {
    use super::{is_date_string, is_datetime_string};

    #[test]
    fn datetime_accepts_common_iso_and_yaml_forms() {
        for value in [
            "2026-02-13T00:00",
            "2026-02-13T00:00:00",
            "2026-02-13T00:00:00.000Z",
            "2026-02-13T00:00:00.000+00:00",
            "2026-02-13T23:59:59-05:00",
            "2026-02-13 00:00:00+00:00",
        ] {
            assert!(is_datetime_string(value), "{value} should be a datetime");
        }
    }

    #[test]
    fn datetime_rejects_invalid_dates_times_and_offsets() {
        for value in [
            "2026-02-30T00:00",
            "2026-02-13",
            "2026-02-13T24:00",
            "2026-02-13T00:60",
            "2026-02-13T00:00:60",
            "2026-02-13T00:00:00.",
            "2026-02-13T00:00:00+2:00",
        ] {
            assert!(!is_datetime_string(value), "{value} should be invalid");
        }
    }

    #[test]
    fn date_accepts_plain_dates_and_yaml_midnight_normalization() {
        for value in [
            "2026-03-20",
            "2026-03-20 00:00:00+00:00",
            "2026-03-20T00:00:00.000Z",
            "2024-02-29",
        ] {
            assert!(is_date_string(value), "{value} should be a date");
        }
    }

    #[test]
    fn date_rejects_invalid_dates_and_non_midnight_datetimes() {
        for value in [
            "2026-02-29",
            "2026-03-20 00:01:00+00:00",
            "2026-03-20T12:00:00Z",
            "2026-13-20",
            "2026-03-32",
        ] {
            assert!(!is_date_string(value), "{value} should be invalid");
        }
    }
}
