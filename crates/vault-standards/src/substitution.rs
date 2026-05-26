//! Render a NaiveDateTime through a Moment-subset format string.
//!
//! Supported tokens (v1):
//! - Year: YYYY, YY
//! - Month: MM, M, MMM, MMMM
//! - Day: DD, D
//! - Hour: HH, H, hh, h
//! - Minute: mm
//! - Second: ss
//! - AM/PM: A, a
//! - Day of week: dddd, ddd
//!
//! Bracket escape: `[X]` renders X as literal (where X may be a token letter).
//! An unmatched `[` (no closing `]`) is emitted as a literal `[` and scanning continues.
//! Unsupported token-shaped sequences render as themselves.
//!
//! This module is the format-token layer of the substitution engine; variable
//! resolution (`{{var}}`) and pipe transforms (`{{var | transform}}`) are layered
//! in Tasks 1.2/1.3 of the vault-new arc.

use chrono::{Datelike, NaiveDateTime, Timelike};
use std::collections::BTreeMap;

/// Known format tokens.
///
/// Variants are declared longest-prefix-first within each family so that a
/// linear scan of `Token::ALL` correctly applies longest-match. When adding a
/// new token that shares a leading prefix with an existing one, insert it
/// *before* the shorter sibling (e.g. MMMM before MMM before MM before M).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token {
    /// `MMMM` — full month name
    Mmmm,
    /// `dddd` — full weekday name
    Dddd,
    /// `YYYY` — 4-digit year
    Yyyy,
    /// `MMM` — abbreviated month name
    Mmm,
    /// `ddd` — abbreviated weekday name
    Ddd,
    /// `YY` — 2-digit year
    Yy,
    /// `MM` — zero-padded month
    Mm,
    /// `DD` — zero-padded day
    Dd,
    /// `HH` — zero-padded 24-hour
    Hh24,
    /// `hh` — zero-padded 12-hour
    Hh12,
    /// `mm` — zero-padded minute
    MmMinute,
    /// `ss` — zero-padded second
    Ss,
    /// `M` — bare month
    M,
    /// `D` — bare day
    D,
    /// `H` — bare 24-hour
    H24,
    /// `h` — bare 12-hour
    H12,
    /// `A` — uppercase AM/PM
    AUpper,
    /// `a` — lowercase am/pm
    ALower,
}

impl Token {
    /// All tokens in longest-match-first order.
    const ALL: &'static [(Token, &'static str)] = &[
        (Token::Mmmm, "MMMM"),
        (Token::Dddd, "dddd"),
        (Token::Yyyy, "YYYY"),
        (Token::Mmm, "MMM"),
        (Token::Ddd, "ddd"),
        (Token::Yy, "YY"),
        (Token::Mm, "MM"),
        (Token::Dd, "DD"),
        (Token::Hh24, "HH"),
        (Token::Hh12, "hh"),
        (Token::MmMinute, "mm"),
        (Token::Ss, "ss"),
        (Token::M, "M"),
        (Token::D, "D"),
        (Token::H24, "H"),
        (Token::H12, "h"),
        (Token::AUpper, "A"),
        (Token::ALower, "a"),
    ];

    fn render(self, t: &NaiveDateTime) -> String {
        match self {
            Token::Yyyy => format!("{:04}", t.year()),
            Token::Yy => format!("{:02}", t.year() % 100),
            Token::Mm => format!("{:02}", t.month()),
            Token::M => format!("{}", t.month()),
            Token::Mmm => MONTH_ABBR[(t.month() - 1) as usize].to_string(),
            Token::Mmmm => MONTH_FULL[(t.month() - 1) as usize].to_string(),
            Token::Dd => format!("{:02}", t.day()),
            Token::D => format!("{}", t.day()),
            Token::Hh24 => format!("{:02}", t.hour()),
            Token::H24 => format!("{}", t.hour()),
            Token::Hh12 => format!("{:02}", hour_12(t.hour())),
            Token::H12 => format!("{}", hour_12(t.hour())),
            Token::MmMinute => format!("{:02}", t.minute()),
            Token::Ss => format!("{:02}", t.second()),
            Token::AUpper => (if t.hour() < 12 { "AM" } else { "PM" }).to_string(),
            Token::ALower => (if t.hour() < 12 { "am" } else { "pm" }).to_string(),
            Token::Ddd => DOW_ABBR[t.weekday().num_days_from_monday() as usize].to_string(),
            Token::Dddd => DOW_FULL[t.weekday().num_days_from_monday() as usize].to_string(),
        }
    }
}

/// Render a [`NaiveDateTime`] through a Moment-subset format string.
pub fn format_datetime(fmt: &str, t: &NaiveDateTime) -> String {
    let mut out = String::with_capacity(fmt.len() + 8);
    let bytes = fmt.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Bracket-escape: `[` and `]` are ASCII, so byte indexing is safe here.
        if bytes[i] == b'[' {
            if let Some(end) = fmt[i + 1..].find(']') {
                out.push_str(&fmt[i + 1..i + 1 + end]);
                i += end + 2;
                continue;
            }
            // Unmatched `[`: emit literal and continue.
            out.push('[');
            i += 1;
            continue;
        }
        // Try to match a known token at this position.
        let remaining = &fmt[i..];
        if let Some((tok, src_len)) = try_token(remaining, t) {
            out.push_str(&tok);
            i += src_len;
        } else {
            // Fallthrough: emit the next Unicode scalar value as a literal.
            // `remaining` is a valid &str, so chars() is safe and advances by
            // the correct number of bytes for multi-byte codepoints.
            let ch = remaining.chars().next().expect("non-empty remaining");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Try to match a format token at the start of `s`.
///
/// Returns `(rendered_output, source_byte_length)` on success.
fn try_token(s: &str, t: &NaiveDateTime) -> Option<(String, usize)> {
    for &(token, pattern) in Token::ALL {
        if s.starts_with(pattern) {
            return Some((token.render(t), pattern.len()));
        }
    }
    None
}

fn hour_12(h: u32) -> u32 {
    let h = h % 12;
    if h == 0 {
        12
    } else {
        h
    }
}

const MONTH_ABBR: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const MONTH_FULL: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const DOW_ABBR: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const DOW_FULL: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

// ── Variable resolution ────────────────────────────────────────────────────

/// Substitution context: resolved once per `vault new` invocation.
#[derive(Debug, Clone)]
pub struct Context {
    pub now: NaiveDateTime,
    pub title: String,
    pub path_vars: BTreeMap<String, String>,
    pub date_format: String,
    pub time_format: String,
}

/// Errors produced by [`render`].
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("unknown variable `{0}`")]
    UnknownVariable(String),
    #[error("unknown transform `{0}`")]
    UnknownTransform(String),
    #[error("malformed template: {0}")]
    Malformed(String),
}

/// Render a template string against the context.
///
/// `{{{{` renders as a literal `{{`; `}}}}` renders as `}}`.
/// Unknown `{{path.X}}` variables render as empty string — the caller
/// surfaces a `path_variable_unresolved` warning.
pub fn render(template: &str, ctx: &Context) -> Result<String, RenderError> {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // `{{{{` → literal `{{`
        if i + 3 < bytes.len()
            && bytes[i] == b'{'
            && bytes[i + 1] == b'{'
            && bytes[i + 2] == b'{'
            && bytes[i + 3] == b'{'
        {
            out.push_str("{{");
            i += 4;
            continue;
        }
        // `}}}}` → literal `}}`
        if i + 3 < bytes.len()
            && bytes[i] == b'}'
            && bytes[i + 1] == b'}'
            && bytes[i + 2] == b'}'
            && bytes[i + 3] == b'}'
        {
            out.push_str("}}");
            i += 4;
            continue;
        }
        // `{{ … }}` substitution
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            if let Some(end) = template[i + 2..].find("}}") {
                let inner = &template[i + 2..i + 2 + end];
                let rendered = render_expression(inner.trim(), ctx)?;
                out.push_str(&rendered);
                i += end + 4;
                continue;
            }
            return Err(RenderError::Malformed(format!(
                "unclosed `{{{{` at byte {i}"
            )));
        }
        // Literal char — UTF-8 safe via chars().
        let ch = template[i..]
            .chars()
            .next()
            .expect("non-empty by loop guard");
        out.push(ch);
        i += ch.len_utf8();
    }
    Ok(out)
}

fn render_expression(expr: &str, ctx: &Context) -> Result<String, RenderError> {
    let mut parts = expr.split('|').map(str::trim);
    let var_part = parts.next().expect("split always yields at least one part");
    let mut value = render_var(var_part, ctx)?;
    for transform_name in parts {
        value = apply_transform(transform_name, &value)?;
    }
    Ok(value)
}

fn apply_transform(name: &str, value: &str) -> Result<String, RenderError> {
    match name {
        "titlecase" => Ok(titlecase(value)),
        "sentencecase" => Ok(sentencecase(value)),
        "lower" => Ok(value.to_lowercase()),
        "upper" => Ok(value.to_uppercase()),
        "unsep" => Ok(unsep(value)),
        "strip_date_prefix" => Ok(strip_date_prefix(value)),
        "slugify" => Ok(slugify(value)),
        other => Err(RenderError::UnknownTransform(other.into())),
    }
}

fn titlecase(s: &str) -> String {
    unsep(s)
        .split_whitespace()
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn sentencecase(s: &str) -> String {
    let lowered = unsep(s).to_lowercase();
    let mut chars = lowered.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn unsep(s: &str) -> String {
    s.replace(['-', '_'], " ")
}

fn strip_date_prefix(s: &str) -> String {
    // Match leading YYYY-MM-DD- (11 bytes including trailing hyphen).
    let bytes = s.as_bytes();
    if bytes.len() < 11 {
        return s.into();
    }
    let prefix_shape = bytes[..10].iter().enumerate().all(|(i, b)| match i {
        0..=3 => b.is_ascii_digit(),
        4 => *b == b'-',
        5 | 6 => b.is_ascii_digit(),
        7 => *b == b'-',
        8 | 9 => b.is_ascii_digit(),
        _ => unreachable!(),
    });
    if prefix_shape && bytes[10] == b'-' {
        s[11..].into()
    } else {
        s.into()
    }
}

fn slugify(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut last_was_sep = true;
    for ch in lower.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_was_sep = false;
        } else if !last_was_sep {
            out.push('-');
            last_was_sep = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

fn render_var(expr: &str, ctx: &Context) -> Result<String, RenderError> {
    // Parse `name` or `name:arg`.
    let (name, arg) = match expr.find(':') {
        Some(idx) => (&expr[..idx], Some(&expr[idx + 1..])),
        None => (expr, None),
    };
    let name = name.trim();
    match name {
        "title" => Ok(ctx.title.clone()),
        "now" => Ok(format_datetime("YYYY-MM-DDTHH:mm", &ctx.now)),
        "date" => {
            let fmt = arg.unwrap_or(&ctx.date_format);
            Ok(format_datetime(fmt, &ctx.now))
        }
        "time" => {
            let fmt = arg.unwrap_or(&ctx.time_format);
            Ok(format_datetime(fmt, &ctx.now))
        }
        n if n.starts_with("path.") => {
            let key = &n["path.".len()..];
            // Empty string for unknown path var — caller surfaces warning.
            Ok(ctx.path_vars.get(key).cloned().unwrap_or_default())
        }
        other => Err(RenderError::UnknownVariable(other.into())),
    }
}

#[cfg(test)]
mod var_tests {
    use super::*;
    use chrono::{NaiveDate, NaiveTime};
    use std::collections::BTreeMap;

    fn ctx_for(stem: &str) -> Context {
        let now = NaiveDate::from_ymd_opt(2026, 5, 25)
            .unwrap()
            .and_time(NaiveTime::from_hms_opt(18, 30, 45).unwrap());
        Context {
            now,
            title: stem.into(),
            path_vars: BTreeMap::new(),
            date_format: "YYYY-MM-DD".into(),
            time_format: "HH:mm".into(),
        }
    }

    #[test]
    fn renders_title_var() {
        let ctx = ctx_for("design-vault-new");
        assert_eq!(render("{{title}}", &ctx).unwrap(), "design-vault-new");
    }

    #[test]
    fn renders_date_default_format() {
        let ctx = ctx_for("foo");
        assert_eq!(render("{{date}}", &ctx).unwrap(), "2026-05-25");
    }

    #[test]
    fn renders_time_default_format() {
        let ctx = ctx_for("foo");
        assert_eq!(render("{{time}}", &ctx).unwrap(), "18:30");
    }

    #[test]
    fn renders_date_custom_format() {
        let ctx = ctx_for("foo");
        assert_eq!(
            render("{{date:YYYY-MM-DDTHH:mm}}", &ctx).unwrap(),
            "2026-05-25T18:30"
        );
    }

    #[test]
    fn renders_time_custom_format() {
        let ctx = ctx_for("foo");
        assert_eq!(render("{{time:HH:mm:ss}}", &ctx).unwrap(), "18:30:45");
    }

    #[test]
    fn renders_now_iso_extension() {
        let ctx = ctx_for("foo");
        assert_eq!(render("{{now}}", &ctx).unwrap(), "2026-05-25T18:30");
    }

    #[test]
    fn renders_path_var() {
        let mut ctx = ctx_for("foo");
        ctx.path_vars.insert("workspace".into(), "vault-cli".into());
        assert_eq!(
            render("[[{{path.workspace}}]]", &ctx).unwrap(),
            "[[vault-cli]]"
        );
    }

    #[test]
    fn unknown_path_var_renders_empty() {
        // Empty string for unknown path var — caller emits warning.
        let ctx = ctx_for("foo");
        assert_eq!(render("{{path.unknown}}", &ctx).unwrap(), "");
    }

    #[test]
    fn multiple_vars_in_one_string() {
        let ctx = ctx_for("foo");
        assert_eq!(
            render("created at {{date}} {{time}}", &ctx).unwrap(),
            "created at 2026-05-25 18:30"
        );
    }

    #[test]
    fn literal_braces_via_double_brace_escape() {
        let ctx = ctx_for("foo");
        // `{{{{` renders as literal `{{`; `}}}}` as literal `}}`
        assert_eq!(render("{{{{not a var}}}}", &ctx).unwrap(), "{{not a var}}");
    }

    #[test]
    fn unclosed_brace_errors() {
        let ctx = ctx_for("foo");
        let err = render("{{unclosed", &ctx).unwrap_err();
        assert!(
            matches!(err, RenderError::Malformed(_)),
            "expected Malformed, got: {err:?}"
        );
    }

    #[test]
    fn unknown_var_errors() {
        let ctx = ctx_for("foo");
        let err = render("{{whatever}}", &ctx).unwrap_err();
        assert!(
            matches!(err, RenderError::UnknownVariable(ref name) if name == "whatever"),
            "expected UnknownVariable(\"whatever\"), got: {err:?}"
        );
    }
}

#[cfg(test)]
mod transform_tests {
    use super::*;
    use chrono::{NaiveDate, NaiveTime};
    use std::collections::BTreeMap;

    fn ctx() -> Context {
        Context {
            now: NaiveDate::from_ymd_opt(2026, 5, 25)
                .unwrap()
                .and_time(NaiveTime::from_hms_opt(18, 30, 0).unwrap()),
            title: "design-vault-new".into(),
            path_vars: BTreeMap::new(),
            date_format: "YYYY-MM-DD".into(),
            time_format: "HH:mm".into(),
        }
    }

    #[test]
    fn titlecase_basic() {
        assert_eq!(
            render("{{title | titlecase}}", &ctx()).unwrap(),
            "Design Vault New"
        );
    }

    #[test]
    fn sentencecase_basic() {
        assert_eq!(
            render("{{title | sentencecase}}", &ctx()).unwrap(),
            "Design vault new"
        );
    }

    #[test]
    fn lower_upper() {
        let mut c = ctx();
        c.title = "Vault CLI".into();
        assert_eq!(render("{{title | lower}}", &c).unwrap(), "vault cli");
        assert_eq!(render("{{title | upper}}", &c).unwrap(), "VAULT CLI");
    }

    #[test]
    fn unsep_handles_hyphen_and_underscore() {
        let mut c = ctx();
        c.title = "design-vault_new".into();
        assert_eq!(render("{{title | unsep}}", &c).unwrap(), "design vault new");
    }

    #[test]
    fn strip_date_prefix_strips_iso() {
        let mut c = ctx();
        c.title = "2026-05-25-design-vault-new".into();
        assert_eq!(
            render("{{title | strip_date_prefix}}", &c).unwrap(),
            "design-vault-new"
        );
    }

    #[test]
    fn strip_date_prefix_noop_when_absent() {
        let mut c = ctx();
        c.title = "design-vault-new".into();
        assert_eq!(
            render("{{title | strip_date_prefix}}", &c).unwrap(),
            "design-vault-new"
        );
    }

    #[test]
    fn slugify_basic() {
        let mut c = ctx();
        c.title = "Design Vault New Command!".into();
        assert_eq!(
            render("{{title | slugify}}", &c).unwrap(),
            "design-vault-new-command"
        );
    }

    #[test]
    fn slugify_handles_consecutive_seps() {
        let mut c = ctx();
        c.title = "  a  --  b  ".into();
        assert_eq!(render("{{title | slugify}}", &c).unwrap(), "a-b");
    }

    #[test]
    fn chain_strip_then_titlecase() {
        let mut c = ctx();
        c.title = "2026-05-25-design-vault-new".into();
        assert_eq!(
            render("{{title | strip_date_prefix | titlecase}}", &c).unwrap(),
            "Design Vault New"
        );
    }

    #[test]
    fn unknown_transform_errors() {
        let err = render("{{title | bogus}}", &ctx()).unwrap_err();
        assert!(
            matches!(err, RenderError::UnknownTransform(ref name) if name == "bogus"),
            "expected UnknownTransform(\"bogus\"), got: {err:?}"
        );
    }

    #[test]
    fn chain_arbitrary_order() {
        let mut c = ctx();
        c.title = "Foo Bar".into();
        assert_eq!(
            render("{{title | lower | slugify}}", &c).unwrap(),
            "foo-bar"
        );
    }
}

#[cfg(test)]
mod format_tests {
    use super::format_datetime;
    use chrono::{NaiveDate, NaiveDateTime, NaiveTime};

    fn dt(y: i32, m: u32, d: u32, h: u32, mi: u32, s: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_time(NaiveTime::from_hms_opt(h, mi, s).unwrap())
    }

    #[test]
    fn formats_year_tokens() {
        let t = dt(2026, 5, 25, 18, 30, 0);
        assert_eq!(format_datetime("YYYY", &t), "2026");
        assert_eq!(format_datetime("YY", &t), "26");
    }

    #[test]
    fn formats_month_day_tokens() {
        let t = dt(2026, 5, 25, 18, 30, 0);
        assert_eq!(format_datetime("YYYY-MM-DD", &t), "2026-05-25");
        assert_eq!(format_datetime("M/D", &t), "5/25");
        assert_eq!(format_datetime("MMM D", &t), "May 25");
        assert_eq!(format_datetime("MMMM D", &t), "May 25");
        // January: short and long forms differ, catching any MMM/MMMM routing bug.
        let jan = dt(2026, 1, 15, 12, 0, 0);
        assert_eq!(format_datetime("MMM", &jan), "Jan");
        assert_eq!(format_datetime("MMMM", &jan), "January");
    }

    #[test]
    fn formats_hour_minute_tokens() {
        let t = dt(2026, 5, 25, 18, 30, 45);
        assert_eq!(format_datetime("HH:mm:ss", &t), "18:30:45");
        assert_eq!(format_datetime("h:mm A", &t), "6:30 PM");
        assert_eq!(format_datetime("hh a", &t), "06 pm");
        // Midnight: h=0 → 12 AM
        let midnight = dt(2026, 5, 25, 0, 0, 0);
        assert_eq!(format_datetime("h:mm A", &midnight), "12:00 AM");
        // Noon: h=12 → 12 PM
        let noon = dt(2026, 5, 25, 12, 0, 0);
        assert_eq!(format_datetime("h:mm A", &noon), "12:00 PM");
        assert_eq!(format_datetime("hh a", &noon), "12 pm");
    }

    #[test]
    fn formats_day_of_week_tokens() {
        let t = dt(2026, 5, 25, 18, 30, 0); // Monday
        assert_eq!(format_datetime("ddd", &t), "Mon");
        assert_eq!(format_datetime("dddd", &t), "Monday");
    }

    #[test]
    fn literals_pass_through() {
        let t = dt(2026, 5, 25, 18, 30, 0);
        assert_eq!(
            format_datetime("YYYY-MM-DD'T'HH:mm", &t),
            "2026-05-25'T'18:30"
        );
        assert_eq!(format_datetime("YYYY-MM-DDTHH:mm", &t), "2026-05-25T18:30");
    }

    #[test]
    fn bracket_escape_for_token_letters() {
        let t = dt(2026, 5, 25, 18, 30, 0);
        assert_eq!(format_datetime("[Year] YYYY", &t), "Year 2026");
    }

    #[test]
    fn unsupported_tokens_render_as_literal() {
        let t = dt(2026, 5, 25, 18, 30, 0);
        assert_eq!(format_datetime("Q", &t), "Q");
    }

    #[test]
    fn non_ascii_literals_pass_through() {
        let t = dt(2026, 5, 25, 18, 30, 0);
        // CJK separators: each kanji is 3 bytes; must not panic on byte-boundary.
        assert_eq!(format_datetime("YYYY年MM月DD日", &t), "2026年05月25日");
        // En-dash typographic separator (3 bytes in UTF-8).
        assert_eq!(format_datetime("YYYY–MM–DD", &t), "2026–05–25");
        // Bracket-escaped CJK literal.
        assert_eq!(format_datetime("[今日] YYYY-MM-DD", &t), "今日 2026-05-25");
    }
}
