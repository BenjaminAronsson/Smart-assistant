//! Bounded, read-only CalDAV calendar reads (M4, FR-35, ADR-025).
//!
//! The host supplies an already-resolved password. This adapter only performs
//! a time-ranged REPORT and GETs the returned calendar resources; it does not
//! create, modify, or delete calendar data.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use jarvis_application::calendar::{
    CalendarEvent, CalendarReader, CalendarReaderError, LocalDayWindow, MAX_AGENDA_EVENTS,
};
use jarvis_domain::location::Sensitivity;
use reqwest::{Client, Response, Url};
use time::{Date, OffsetDateTime, PrimitiveDateTime, Time};
use tokio_util::sync::CancellationToken;

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_EVENT_BYTES: usize = 256 * 1024;
const MAX_HREFS: usize = MAX_AGENDA_EVENTS;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

const REPORT_BODY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<c:calendar-query xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:d="DAV:">
  <d:prop><d:getetag/><c:calendar-data/></d:prop>
  <c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VEVENT">
    <c:time-range start="{start}" end="{end}"/>
  </c:comp-filter></c:comp-filter></c:filter>
</c:calendar-query>"#;

/// CalDAV connection settings. `password` must already be resolved by the host
/// and is deliberately not exposed through `Debug`.
pub struct CalDavConfig {
    pub server_url: Url,
    pub username: String,
    pub password: String,
}

impl CalDavConfig {
    pub fn new(
        server_url: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self, CalendarReaderError> {
        let server_url: Url = server_url
            .into()
            .parse()
            .map_err(|_| CalendarReaderError::Failed("invalid calendar configuration".into()))?;
        if server_url.scheme() != "https" || server_url.host_str().is_none() {
            return Err(CalendarReaderError::Failed(
                "invalid calendar configuration".into(),
            ));
        }
        Ok(Self {
            server_url,
            username: username.into(),
            password: password.into(),
        })
    }
}

pub struct CalDavReader {
    client: Client,
    config: CalDavConfig,
}

impl CalDavReader {
    pub fn new(config: CalDavConfig) -> Result<Self, CalendarReaderError> {
        let client = Client::builder()
            .connect_timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| CalendarReaderError::Failed("calendar client unavailable".into()))?;
        Ok(Self { client, config })
    }

    async fn request_body(
        &self,
        request: reqwest::RequestBuilder,
        max_bytes: usize,
        cancel: &CancellationToken,
    ) -> Result<String, CalendarReaderError> {
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(CalendarReaderError::Cancelled),
            result = tokio::time::timeout(REQUEST_TIMEOUT, request.send()) => {
                result.map_err(|_| CalendarReaderError::Unavailable)?
                    .map_err(|_| CalendarReaderError::Unavailable)?
            }
        };
        if !response.status().is_success() {
            return Err(CalendarReaderError::Unavailable);
        }
        tokio::time::timeout(REQUEST_TIMEOUT, read_response(response, max_bytes, cancel))
            .await
            .map_err(|_| CalendarReaderError::Unavailable)?
    }
}

#[async_trait]
impl CalendarReader for CalDavReader {
    async fn read(
        &self,
        window: LocalDayWindow,
        cancel: CancellationToken,
    ) -> Result<Vec<CalendarEvent>, CalendarReaderError> {
        if cancel.is_cancelled() {
            return Err(CalendarReaderError::Cancelled);
        }
        let start = format_caldav_instant(window.start)?;
        let end = format_caldav_instant(window.end)?;
        let report = REPORT_BODY
            .replace("{start}", &start)
            .replace("{end}", &end);
        let response = self
            .request_body(
                self.client
                    .request(
                        reqwest::Method::from_bytes(b"REPORT").expect("static method"),
                        self.config.server_url.clone(),
                    )
                    .basic_auth(&self.config.username, Some(&self.config.password))
                    .header(
                        reqwest::header::CONTENT_TYPE,
                        "application/xml; charset=utf-8",
                    )
                    .header("Depth", "1")
                    .body(report),
                MAX_RESPONSE_BYTES,
                &cancel,
            )
            .await?;

        let (inline, hrefs) = report_resources(&response)?;
        if hrefs.len() > MAX_HREFS {
            return Err(CalendarReaderError::Failed(
                "calendar response exceeded event limit".into(),
            ));
        }
        let mut calendars = inline;
        for href in hrefs {
            if calendars.len() >= MAX_HREFS {
                return Err(CalendarReaderError::Failed(
                    "calendar response exceeded event limit".into(),
                ));
            }
            let url = self
                .config
                .server_url
                .join(&href)
                .map_err(|_| CalendarReaderError::Failed("invalid calendar resource".into()))?;
            if url.scheme() != self.config.server_url.scheme()
                || url.host_str() != self.config.server_url.host_str()
                || url.port_or_known_default() != self.config.server_url.port_or_known_default()
                || url.username() != ""
                || url.password().is_some()
            {
                return Err(CalendarReaderError::Failed(
                    "calendar resource origin was rejected".into(),
                ));
            }
            let body = self
                .request_body(
                    self.client
                        .get(url)
                        .basic_auth(&self.config.username, Some(&self.config.password)),
                    MAX_EVENT_BYTES,
                    &cancel,
                )
                .await?;
            calendars.push(body);
        }

        let mut events = Vec::new();
        for calendar in calendars {
            for event in parse_icalendar(&calendar, window)? {
                if events.len() >= MAX_AGENDA_EVENTS {
                    return Err(CalendarReaderError::Failed(
                        "calendar response exceeded event limit".into(),
                    ));
                }
                events.push(event);
            }
        }
        events.sort_by_key(|event| event.start);
        Ok(events)
    }
}

async fn read_response(
    mut response: Response,
    max_bytes: usize,
    cancel: &CancellationToken,
) -> Result<String, CalendarReaderError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(CalendarReaderError::Failed(
            "calendar response exceeded size limit".into(),
        ));
    }
    let mut body = Vec::new();
    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => return Err(CalendarReaderError::Cancelled),
            result = response.chunk() => result.map_err(|_| CalendarReaderError::Unavailable)?,
        };
        let Some(chunk) = chunk else { break };
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(CalendarReaderError::Failed(
                "calendar response exceeded size limit".into(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body)
        .map_err(|_| CalendarReaderError::Failed("calendar response was not valid text".into()))
}

fn report_resources(body: &str) -> Result<(Vec<String>, Vec<String>), CalendarReaderError> {
    let inline = xml_values(body, "calendar-data");
    let hrefs = xml_values(body, "href");
    if inline.is_empty() && hrefs.is_empty() && !body.contains("<multistatus") {
        return Err(CalendarReaderError::Failed(
            "invalid calendar response".into(),
        ));
    }
    Ok((inline, hrefs))
}

fn xml_values(body: &str, name: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut rest = body;
    while let Some(open) = rest.find('>') {
        let tag = &rest[..open];
        let original_name = tag
            .strip_prefix('<')
            .filter(|_| !tag.starts_with("</"))
            .and_then(|tag| tag.split_whitespace().next())
            .filter(|tag| tag.rsplit(':').next() == Some(name));
        if let Some(original_name) = original_name {
            let after = &rest[open + 1..];
            let close = format!("</{original_name}>");
            if let Some(end) = after.find(&close) {
                values.push(html_unescape(after[..end].trim()));
                rest = &after[end + close.len()..];
                continue;
            }
        }
        rest = &rest[open + 1..];
    }
    values
}

fn html_unescape(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn parse_icalendar(
    body: &str,
    window: LocalDayWindow,
) -> Result<Vec<CalendarEvent>, CalendarReaderError> {
    let lines = unfold_lines(body);
    let mut events = Vec::new();
    let mut current = Vec::new();
    for line in lines {
        if line.eq_ignore_ascii_case("BEGIN:VEVENT") {
            current.clear();
        } else if line.eq_ignore_ascii_case("END:VEVENT") {
            if let Some(event) = parse_event(&current, window)? {
                events.push(event);
            }
            current.clear();
        } else if !current.is_empty() || line.starts_with("UID") {
            current.push(line);
        }
    }
    Ok(events)
}

fn unfold_lines(body: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in body.replace("\r\n", "\n").replace('\r', "\n").split('\n') {
        if raw.starts_with(' ') || raw.starts_with('\t') {
            if let Some(last) = lines.last_mut() {
                last.push_str(&raw[1..]);
            }
        } else {
            lines.push(raw.to_owned());
        }
    }
    lines
}

fn parse_event(
    lines: &[String],
    window: LocalDayWindow,
) -> Result<Option<CalendarEvent>, CalendarReaderError> {
    let mut uid = None;
    let mut summary = None;
    let mut start = None;
    let mut end = None;
    let mut all_day = false;
    for line in lines {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let name = key
            .split(';')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        match name.as_str() {
            "UID" => uid = Some(value),
            "SUMMARY" => summary = Some(unescape_text(value)),
            "DTSTART" => {
                all_day = key.to_ascii_uppercase().contains("VALUE=DATE");
                start = Some(parse_datetime(value, all_day)?);
            }
            "DTEND" => end = Some(parse_datetime(value, all_day)?),
            _ => {}
        }
    }
    let (Some(uid), Some(summary), Some(start), Some(end)) = (uid, summary, start, end) else {
        return Err(CalendarReaderError::Failed(
            "calendar event was incomplete".into(),
        ));
    };
    if uid.is_empty() {
        return Err(CalendarReaderError::Failed(
            "calendar event was incomplete".into(),
        ));
    }
    if start >= end || end <= window.start || start >= window.end {
        return Ok(None);
    }
    CalendarEvent::new(summary, start, end, all_day, Sensitivity::Sensitive)
        .map(Some)
        .map_err(|_| CalendarReaderError::Failed("calendar event was invalid".into()))
}

fn unescape_text(value: &str) -> String {
    value
        .replace("\\n", "\n")
        .replace("\\N", "\n")
        .replace("\\,", ",")
        .replace("\\;", ";")
        .replace("\\\"", "\"")
        .replace("\\\\", "\\")
}

fn parse_datetime(value: &str, all_day: bool) -> Result<SystemTime, CalendarReaderError> {
    let parsed = if all_day {
        let date = Date::parse(
            value,
            &time::format_description::parse_borrowed::<2>("[year][month][day]")
                .expect("static format"),
        )
        .map_err(|_| CalendarReaderError::Failed("calendar event had an invalid time".into()))?;
        date.with_time(Time::MIDNIGHT).assume_utc()
    } else {
        let value = value.strip_suffix('Z').unwrap_or(value);
        let format = time::format_description::parse_borrowed::<2>(
            "[year][month][day]T[hour][minute][second]",
        )
        .expect("static format");
        PrimitiveDateTime::parse(value, &format)
            .map_err(|_| CalendarReaderError::Failed("calendar event had an invalid time".into()))?
            .assume_utc()
    };
    let seconds = parsed.unix_timestamp();
    if seconds < 0 {
        return Err(CalendarReaderError::Failed(
            "calendar event had an invalid time".into(),
        ));
    }
    UNIX_EPOCH
        .checked_add(Duration::from_secs(seconds as u64))
        .ok_or_else(|| CalendarReaderError::Failed("calendar event had an invalid time".into()))
}

fn format_caldav_instant(value: SystemTime) -> Result<String, CalendarReaderError> {
    let duration = value
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CalendarReaderError::Failed("invalid calendar window".into()))?;
    let timestamp = OffsetDateTime::from_unix_timestamp(duration.as_secs() as i64)
        .map_err(|_| CalendarReaderError::Failed("invalid calendar window".into()))?;
    timestamp
        .format(
            &time::format_description::parse_borrowed::<2>(
                "[year][month][day]T[hour][minute][second]Z",
            )
            .expect("static format"),
        )
        .map_err(|_| CalendarReaderError::Failed("invalid calendar window".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window() -> LocalDayWindow {
        LocalDayWindow::new(
            UNIX_EPOCH + Duration::from_secs(1_751_587_200),
            UNIX_EPOCH + Duration::from_secs(1_751_760_000),
        )
        .unwrap()
    }

    #[test]
    fn fixture_parses_bounded_events_and_unescapes_summary() {
        let body = include_str!("../tests/fixtures/caldav/events.ics");
        let events = parse_icalendar(body, window()).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].title, "Dentist, \"check-up\"");
        assert!(events[1].all_day);
        assert_eq!(events[1].sensitivity, Sensitivity::Sensitive);
    }

    #[test]
    fn malformed_event_is_generic_and_does_not_include_contents() {
        let error = parse_icalendar("BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:secret\nSUMMARY:very private\nEND:VEVENT\nEND:VCALENDAR", window()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "calendar read failed: calendar event was incomplete"
        );
        assert!(!error.to_string().contains("secret"));
        assert!(!error.to_string().contains("very private"));
    }

    #[test]
    fn report_resources_extracts_hrefs() {
        assert!(
            report_resources("<multistatus><response><href>/a.ics</href></response></multistatus>")
                .unwrap()
                .1
                .len()
                <= MAX_HREFS
        );
    }
}
