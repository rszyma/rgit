// sorry clippy, we don't have a choice. askama forces this on us
#![allow(clippy::unnecessary_wraps, clippy::trivially_copy_pass_by_ref)]

use std::borrow::Borrow;

use time::{format_description::well_known::Rfc3339, Duration};

pub fn format_time(s: impl Borrow<time::OffsetDateTime>) -> Result<String, askama::Error> {
    (*s.borrow())
        .format(&Rfc3339)
        .map_err(Box::from)
        .map_err(askama::Error::Custom)
}

pub fn timeago(s: impl Borrow<time::OffsetDateTime>) -> Result<String, askama::Error> {
    let elapsed: Duration = time::OffsetDateTime::now_utc() - *s.borrow();
    let mut formatter: &mut _ = &mut timeago::Formatter::new();
    let selected_class = if elapsed < 2 * Duration::HOUR {
        formatter = formatter.max_unit(timeago::TimeUnit::Minutes);
        "age-mins"
    } else if elapsed < 2 * Duration::DAY {
        formatter = formatter.max_unit(timeago::TimeUnit::Hours);
        "age-hours"
    } else if elapsed < 14 * Duration::DAY {
        formatter = formatter.max_unit(timeago::TimeUnit::Days);
        "age-days"
    } else if elapsed < 60 * Duration::DAY {
        formatter = formatter.max_unit(timeago::TimeUnit::Weeks);
        "age-weeks"
    } else if elapsed < 2 * 365 * Duration::DAY {
        formatter = formatter.max_unit(timeago::TimeUnit::Months);
        "age-months"
    } else {
        formatter = formatter.max_unit(timeago::TimeUnit::Years);
        "age-years"
    };
    let formatted_time = formatter.convert(elapsed.unsigned_abs());
    Ok(format!(
        r#"<span class="{selected_class}">{formatted_time}</span>"#
    ))
}

pub fn file_perms(s: &i32) -> Result<String, askama::Error> {
    Ok(unix_mode::to_string(s.unsigned_abs()))
}

pub fn hex(s: &[u8]) -> Result<String, askama::Error> {
    Ok(hex::encode(s))
}

pub fn md5(s: &str) -> Result<String, askama::Error> {
    Ok(hex::encode(md5::compute(s).0))
}

#[allow(dead_code)]
pub fn md(md: &str) -> Result<String, askama::Error> {
    Ok(comrak::markdown_to_html(
        md,
        &comrak::ComrakOptions::default(),
    ))
}

pub fn limit80(s: &str) -> Result<String, askama::Error> {
    Ok(if s.len() > 80 {
        format!("{}{}", &s[..77], "...")
    } else {
        s.to_owned()
    })
}
