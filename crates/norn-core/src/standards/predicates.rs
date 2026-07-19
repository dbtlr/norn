//! Field-type and selector predicate semantics — what a declared `field_types`
//! entry or a `match.frontmatter` selector *means*.
//!
//! This is the declaration side of the standards model: the pure value/type
//! predicates the config layer needs to compile and conflict-check rules
//! (`frontmatter_predicate_matches`, `frontmatter_type_matches`, the date/time
//! recognizers). The document-MATCHING helpers (running these predicates over a
//! `Document` to decide which rules fire) belong to the validate engine and are
//! deferred to that port — nothing here depends on the domain model.

use serde_json::Value;

pub(crate) fn frontmatter_value_matches(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::String(actual), Value::String(expected)) => actual == expected,
        (Value::Bool(actual), Value::Bool(expected)) => actual == expected,
        (Value::Number(actual), Value::Number(expected)) => actual == expected,
        _ => false,
    }
}

/// Selector semantics for `match.frontmatter` predicates: a scalar expected
/// value is exact equality; a list is any-of over its scalar elements. The
/// list enumerates candidate values — it is not containment over an
/// array-valued document field.
pub(crate) fn frontmatter_predicate_matches(actual: &Value, expected: &Value) -> bool {
    match expected {
        Value::Array(options) => options
            .iter()
            .any(|option| frontmatter_value_matches(actual, option)),
        _ => frontmatter_value_matches(actual, expected),
    }
}

/// Does `value` satisfy `expected_type`? `max_length` is the bound applied
/// to `string` (the whole value) and `list_of_strings` (each element); it is
/// ignored for every other type.
pub fn frontmatter_type_matches(
    value: &Value,
    expected_type: &str,
    max_length: Option<u32>,
) -> bool {
    match expected_type {
        "datetime" => value.as_str().is_some_and(is_datetime_string),
        "date" => value.as_str().is_some_and(is_date_string),
        "list_of_strings" => value.as_array().is_some_and(|values| {
            values.iter().all(|value| {
                value
                    .as_str()
                    .is_some_and(|s| within_max_length(s, max_length))
            })
        }),
        "wikilink" => value.as_str().is_some_and(is_wikilink_string),
        "wikilink_or_list" => {
            value.as_str().is_some_and(is_wikilink_string)
                || value.as_array().is_some_and(|values| {
                    values
                        .iter()
                        .all(|value| value.as_str().is_some_and(is_wikilink_string))
                })
        }
        "string" => value
            .as_str()
            .is_some_and(|s| within_max_length(s, max_length)),
        "text" => value.as_str().is_some(),
        _ => false,
    }
}

fn within_max_length(value: &str, max_length: Option<u32>) -> bool {
    match max_length {
        Some(bound) => value.chars().count() <= bound as usize,
        None => true,
    }
}

/// Does `value` have the right shape for `expected_type` (`string` or
/// `list_of_strings`) but violate `max_length`? Returns the offending length
/// (the value's length for `string`, the longest element's length for
/// `list_of_strings`) when so; `None` when the value doesn't match the type's
/// base shape at all (a real type mismatch takes priority) or satisfies the
/// bound.
pub fn frontmatter_exceeds_max_length(
    value: &Value,
    expected_type: &str,
    max_length: Option<u32>,
) -> Option<usize> {
    let bound = max_length? as usize;
    match expected_type {
        "string" => {
            let len = value.as_str()?.chars().count();
            (len > bound).then_some(len)
        }
        "list_of_strings" => {
            let values = value.as_array()?;
            let mut max_len = 0;
            for v in values {
                max_len = max_len.max(v.as_str()?.chars().count());
            }
            (max_len > bound).then_some(max_len)
        }
        _ => None,
    }
}

pub fn is_datetime_string(value: &str) -> bool {
    let Some((date, time)) = value.split_once('T').or_else(|| value.split_once(' ')) else {
        return false;
    };

    is_date_string(date) && is_time_string(time)
}

pub fn is_date_string(value: &str) -> bool {
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

pub fn is_wikilink_string(value: &str) -> bool {
    value.starts_with("[[") && value.ends_with("]]") && value.len() > 4
}

#[cfg(test)]
mod tests {
    use super::{
        frontmatter_exceeds_max_length, frontmatter_type_matches, is_date_string,
        is_datetime_string,
    };
    use serde_json::json;

    #[test]
    fn string_type_accepts_within_bound() {
        assert!(frontmatter_type_matches(&json!("abc"), "string", Some(3)));
    }

    #[test]
    fn string_type_rejects_over_bound() {
        assert!(!frontmatter_type_matches(&json!("abcd"), "string", Some(3)));
    }

    #[test]
    fn string_type_rejects_non_string_value() {
        assert!(!frontmatter_type_matches(&json!(3), "string", Some(64)));
    }

    #[test]
    fn string_type_with_no_bound_accepts_any_length() {
        let long = "x".repeat(1000);
        assert!(frontmatter_type_matches(&json!(long), "string", None));
    }

    #[test]
    fn text_type_accepts_any_length_string() {
        let long = "x".repeat(10_000);
        assert!(frontmatter_type_matches(&json!(long), "text", None));
        assert!(frontmatter_type_matches(&json!(long), "text", Some(64)));
    }

    #[test]
    fn text_type_rejects_non_string_value() {
        assert!(!frontmatter_type_matches(&json!(3), "text", None));
    }

    #[test]
    fn list_of_strings_bounds_each_element() {
        assert!(frontmatter_type_matches(
            &json!(["ab", "cd"]),
            "list_of_strings",
            Some(2)
        ));
        assert!(!frontmatter_type_matches(
            &json!(["ab", "cde"]),
            "list_of_strings",
            Some(2)
        ));
    }

    #[test]
    fn list_of_strings_with_no_bound_accepts_any_length_elements() {
        let long = "x".repeat(1000);
        assert!(frontmatter_type_matches(
            &json!([long.clone()]),
            "list_of_strings",
            None
        ));
    }

    #[test]
    fn unknown_type_rejects() {
        assert!(!frontmatter_type_matches(&json!("x"), "bogus", None));
    }

    #[test]
    fn exceeds_max_length_returns_length_for_over_bound_string() {
        assert_eq!(
            frontmatter_exceeds_max_length(&json!("abcd"), "string", Some(3)),
            Some(4)
        );
    }

    #[test]
    fn exceeds_max_length_none_for_within_bound_string() {
        assert_eq!(
            frontmatter_exceeds_max_length(&json!("abc"), "string", Some(3)),
            None
        );
    }

    #[test]
    fn exceeds_max_length_none_for_wrong_type() {
        // A real type mismatch is not a length violation.
        assert_eq!(
            frontmatter_exceeds_max_length(&json!(3), "string", Some(3)),
            None
        );
    }

    #[test]
    fn exceeds_max_length_none_when_no_bound() {
        let long = "x".repeat(1000);
        assert_eq!(
            frontmatter_exceeds_max_length(&json!(long), "string", None),
            None
        );
    }

    #[test]
    fn exceeds_max_length_returns_longest_offending_list_element() {
        assert_eq!(
            frontmatter_exceeds_max_length(&json!(["ab", "abcd"]), "list_of_strings", Some(2)),
            Some(4)
        );
    }

    #[test]
    fn exceeds_max_length_none_for_list_with_non_string_element() {
        // A non-string element is a base type mismatch, not a length violation.
        assert_eq!(
            frontmatter_exceeds_max_length(&json!(["ab", 3]), "list_of_strings", Some(1)),
            None
        );
    }

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
