use std::{collections::BTreeMap, mem, sync::LazyLock};

use anyhow::{Context as _, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Method, Request};
use rand::{Rng as _, SeedableRng as _, rngs::StdRng};
use regex::Regex;
use serde::Serialize;
use serde_json::{Value, json};
use util::ResultExt as _;

pub static SENTRY_DSN: LazyLock<Option<SentryDsn>> = LazyLock::new(|| {
    let dsn = option_env!("ZED_SENTRY_DSN")
        .map(str::to_string)
        .or_else(|| std::env::var("ZED_SENTRY_DSN").ok())?;
    SentryDsn::parse(&dsn).log_err()
});

static DSN_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^https://([0-9a-f]+)@([^/]+)/([0-9]+)$").unwrap());

/// Sentry rejects `event`, `feedback` and `log` items above 1 MiB.
const MAX_ITEM_BYTES: usize = 1024 * 1024;
const LOG_ITEM_OVERHEAD_BYTES: usize = 64;
const MAX_LOGS_PER_ITEM: usize = 100;
/// Sentry truncates feedback messages above 4096 characters.
pub const MAX_FEEDBACK_MESSAGE_CHARS: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentryDsn {
    key: String,
    host: String,
    project_id: u64,
}

impl SentryDsn {
    pub fn parse(dsn: &str) -> Result<Self> {
        let captures = DSN_PATTERN
            .captures(dsn.trim())
            .context("ZED_SENTRY_DSN is not of the form https://<key>@<host>/<project>")?;
        Ok(Self {
            key: captures[1].to_string(),
            host: captures[2].to_string(),
            project_id: captures[3]
                .parse()
                .context("ZED_SENTRY_DSN project id is out of range")?,
        })
    }

    pub fn minidump_url(&self) -> String {
        format!(
            "https://{}/api/{}/minidump/?sentry_key={}",
            self.host, self.project_id, self.key
        )
    }

    pub fn envelope_url(&self) -> String {
        format!(
            "https://{}/api/{}/envelope/?sentry_key={}&sentry_version=7",
            self.host, self.project_id, self.key
        )
    }
}

pub fn new_id() -> String {
    format!("{:032x}", StdRng::from_os_rng().random::<u128>())
}

pub fn timestamp(time: DateTime<Utc>) -> f64 {
    time.timestamp_micros() as f64 / 1_000_000.0
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "lowercase")]
pub enum Attribute {
    String(String),
    Integer(i64),
    Double(f64),
    Boolean(bool),
}

impl Attribute {
    pub fn from_json(value: Value) -> Option<Self> {
        match value {
            Value::Null => None,
            Value::Bool(value) => Some(Self::Boolean(value)),
            Value::Number(number) => match number.as_i64() {
                Some(integer) => Some(Self::Integer(integer)),
                None => number.as_f64().map(Self::Double),
            },
            Value::String(value) => Some(Self::String(value)),
            value @ (Value::Array(_) | Value::Object(_)) => Some(Self::String(value.to_string())),
        }
    }
}

impl From<String> for Attribute {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for Attribute {
    fn from(value: &str) -> Self {
        Self::String(value.to_string())
    }
}

impl From<bool> for Attribute {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Info,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LogRecord {
    pub timestamp: f64,
    pub trace_id: String,
    pub level: LogLevel,
    pub body: String,
    pub attributes: BTreeMap<String, Attribute>,
}

impl LogRecord {
    fn shrink_largest_attribute(&mut self) -> bool {
        let Some((name, length)) = self
            .attributes
            .iter()
            .filter_map(|(name, attribute)| match attribute {
                Attribute::String(value) => Some((name.clone(), value.len())),
                _ => None,
            })
            .max_by_key(|(_, length)| *length)
        else {
            return false;
        };
        self.attributes.remove(&name);
        self.attributes.insert(
            format!("{name}.bytes"),
            Attribute::Integer(i64::try_from(length).unwrap_or(i64::MAX)),
        );
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EventLevel {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LogEntry {
    pub formatted: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct User {
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SentryEvent {
    pub event_id: String,
    pub timestamp: f64,
    pub platform: &'static str,
    pub level: EventLevel,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logentry: Option<LogEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<User>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fingerprint: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub contexts: BTreeMap<String, Value>,
}

impl SentryEvent {
    pub fn new(level: EventLevel, message: impl Into<String>) -> Self {
        Self {
            event_id: new_id(),
            timestamp: timestamp(Utc::now()),
            platform: "native",
            level,
            logentry: Some(LogEntry {
                formatted: message.into(),
            }),
            release: None,
            environment: None,
            user: None,
            tags: BTreeMap::new(),
            fingerprint: Vec::new(),
            contexts: BTreeMap::new(),
        }
    }

    pub fn feedback(message: &str) -> Self {
        let mut event = Self::new(EventLevel::Info, "User Feedback");
        event.logentry = None;
        event.contexts.insert(
            "feedback".to_string(),
            json!({
                "message": message.chars().take(MAX_FEEDBACK_MESSAGE_CHARS).collect::<String>(),
            }),
        );
        event
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    pub filename: String,
    pub content_type: &'static str,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
enum Item {
    Event(SentryEvent),
    Feedback(SentryEvent),
    Logs(Vec<LogRecord>),
    Attachment(Attachment),
}

impl Item {
    fn encode(&self) -> Result<(Value, Vec<u8>)> {
        Ok(match self {
            Self::Event(event) => {
                let payload = serde_json::to_vec(event)?;
                let header = json!({
                    "type": "event",
                    "length": payload.len(),
                    "content_type": "application/json",
                });
                (header, payload)
            }
            Self::Feedback(event) => {
                let payload = serde_json::to_vec(event)?;
                let header = json!({
                    "type": "feedback",
                    "length": payload.len(),
                    "content_type": "application/json",
                });
                (header, payload)
            }
            Self::Logs(records) => {
                let payload = serde_json::to_vec(&json!({ "items": records }))?;
                let header = json!({
                    "type": "log",
                    "length": payload.len(),
                    "item_count": records.len(),
                    "content_type": "application/vnd.sentry.items.log+json",
                });
                (header, payload)
            }
            Self::Attachment(attachment) => {
                let header = json!({
                    "type": "attachment",
                    "length": attachment.data.len(),
                    "filename": attachment.filename,
                    "content_type": attachment.content_type,
                });
                (header, attachment.data.clone())
            }
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Envelope {
    event_id: String,
    items: Vec<Item>,
}

impl Envelope {
    pub fn event(event: SentryEvent) -> Self {
        Self {
            event_id: event.event_id.clone(),
            items: vec![Item::Event(event)],
        }
    }

    pub fn feedback(event: SentryEvent, attachment: Option<Attachment>) -> Self {
        let event_id = event.event_id.clone();
        let mut items = vec![Item::Feedback(event)];
        items.extend(attachment.map(Item::Attachment));
        Self { event_id, items }
    }

    pub fn logs(records: Vec<LogRecord>) -> Self {
        Self {
            event_id: new_id(),
            items: vec![Item::Logs(records)],
        }
    }

    pub fn event_id(&self) -> &str {
        &self.event_id
    }

    pub fn to_bytes(&self, sent_at: DateTime<Utc>) -> Result<Vec<u8>> {
        let mut bytes = serde_json::to_vec(&json!({
            "event_id": self.event_id,
            "sent_at": sent_at.to_rfc3339_opts(SecondsFormat::Millis, true),
        }))?;
        for item in &self.items {
            let (header, payload) = item.encode()?;
            bytes.push(b'\n');
            serde_json::to_writer(&mut bytes, &header)?;
            bytes.push(b'\n');
            bytes.extend_from_slice(&payload);
        }
        bytes.push(b'\n');
        Ok(bytes)
    }
}

pub fn log_envelopes(records: Vec<LogRecord>) -> Result<Vec<Envelope>> {
    let max_batch_bytes = MAX_ITEM_BYTES - LOG_ITEM_OVERHEAD_BYTES;
    let mut envelopes = Vec::new();
    let mut batch = Vec::new();
    let mut batch_bytes = 0;
    for mut record in records {
        let mut record_bytes = serde_json::to_vec(&record)?.len();
        while record_bytes > max_batch_bytes && record.shrink_largest_attribute() {
            record_bytes = serde_json::to_vec(&record)?.len();
        }
        if !batch.is_empty()
            && (batch.len() == MAX_LOGS_PER_ITEM || batch_bytes + record_bytes > max_batch_bytes)
        {
            envelopes.push(Envelope::logs(mem::take(&mut batch)));
            batch_bytes = 0;
        }
        batch_bytes += record_bytes + 1;
        batch.push(record);
    }
    if !batch.is_empty() {
        envelopes.push(Envelope::logs(batch));
    }
    Ok(envelopes)
}

pub async fn send_envelope(
    http: &dyn HttpClient,
    dsn: &SentryDsn,
    envelope: &Envelope,
) -> Result<()> {
    let request = Request::builder()
        .method(Method::POST)
        .uri(dsn.envelope_url())
        .header("Content-Type", "application/x-sentry-envelope")
        .body(AsyncBody::from(envelope.to_bytes(Utc::now())?))?;
    let mut response = http.send(request).await?;
    if !response.status().is_success() {
        let mut body = String::new();
        response.body_mut().read_to_string(&mut body).await?;
        anyhow::bail!(
            "Sentry rejected envelope {}: {} {body}",
            envelope.event_id(),
            response.status()
        );
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
pub const TEST_DSN: &str = "https://0123456789abcdef0123456789abcdef@o1.ingest.sentry.io/4507";

#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub uri: String,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

#[cfg(any(test, feature = "test-support"))]
impl RecordedRequest {
    pub fn envelope_lines(&self) -> Vec<Value> {
        let text = std::str::from_utf8(&self.body).unwrap();
        assert!(text.ends_with('\n'), "envelope must end with a newline");
        text.trim_end_matches('\n')
            .split('\n')
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn recording_http_client() -> (
    std::sync::Arc<http_client::HttpClientWithUrl>,
    std::sync::Arc<parking_lot::Mutex<Vec<RecordedRequest>>>,
) {
    let requests = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let http = http_client::FakeHttpClient::create({
        let requests = requests.clone();
        move |mut request| {
            let requests = requests.clone();
            async move {
                let mut body = Vec::new();
                request.body_mut().read_to_end(&mut body).await?;
                requests.lock().push(RecordedRequest {
                    uri: request.uri().to_string(),
                    content_type: request
                        .headers()
                        .get("Content-Type")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string),
                    body,
                });
                Ok(http_client::Response::builder()
                    .status(200)
                    .body(AsyncBody::default())?)
            }
        }
    });
    (http, requests)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_client::{FakeHttpClient, Response};

    fn dsn() -> SentryDsn {
        SentryDsn::parse(TEST_DSN).unwrap()
    }

    fn record(
        body: &str,
        attributes: impl IntoIterator<Item = (&'static str, Attribute)>,
    ) -> LogRecord {
        LogRecord {
            timestamp: 1_700_000_000.5,
            trace_id: "0".repeat(32),
            level: LogLevel::Info,
            body: body.to_string(),
            attributes: attributes
                .into_iter()
                .map(|(name, attribute)| (name.to_string(), attribute))
                .collect(),
        }
    }

    fn split_envelope(bytes: &[u8]) -> Vec<Value> {
        let text = std::str::from_utf8(bytes).unwrap();
        assert!(text.ends_with('\n'));
        text.trim_end_matches('\n')
            .split('\n')
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn parses_a_dsn() {
        assert_eq!(
            dsn(),
            SentryDsn {
                key: "0123456789abcdef0123456789abcdef".to_string(),
                host: "o1.ingest.sentry.io".to_string(),
                project_id: 4507,
            }
        );
        assert_eq!(SentryDsn::parse(&format!("  {TEST_DSN}\n")).unwrap(), dsn());
    }

    #[test]
    fn rejects_malformed_dsns() {
        for dsn in [
            "https://o1.ingest.sentry.io/4507",
            "https://0123abcd@o1.ingest.sentry.io/project",
            "http://0123abcd@o1.ingest.sentry.io/4507",
            "https://0123abcd@o1.ingest.sentry.io/4507/extra",
            "https://0123abcd@o1.ingest.sentry.io/99999999999999999999999",
        ] {
            assert!(SentryDsn::parse(dsn).is_err(), "{dsn} should not parse");
        }
    }

    #[test]
    fn builds_endpoint_urls() {
        assert_eq!(
            dsn().minidump_url(),
            "https://o1.ingest.sentry.io/api/4507/minidump/?sentry_key=0123456789abcdef0123456789abcdef"
        );
        assert_eq!(
            dsn().envelope_url(),
            "https://o1.ingest.sentry.io/api/4507/envelope/?sentry_key=0123456789abcdef0123456789abcdef&sentry_version=7"
        );
    }

    #[test]
    fn ids_are_32_hex_digits() {
        let id = new_id();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|character| character.is_ascii_hexdigit()));
        assert_ne!(id, new_id());
    }

    #[test]
    fn converts_json_to_typed_attributes() {
        assert_eq!(Attribute::from_json(Value::Null), None);
        assert_eq!(
            Attribute::from_json(json!(true)),
            Some(Attribute::Boolean(true))
        );
        assert_eq!(
            Attribute::from_json(json!(-3)),
            Some(Attribute::Integer(-3))
        );
        assert_eq!(
            Attribute::from_json(json!(1.5)),
            Some(Attribute::Double(1.5))
        );
        assert_eq!(
            Attribute::from_json(json!(u64::MAX)),
            Some(Attribute::Double(u64::MAX as f64))
        );
        assert_eq!(
            Attribute::from_json(json!("text")),
            Some(Attribute::String("text".to_string()))
        );
        assert_eq!(
            Attribute::from_json(json!([1, "a"])),
            Some(Attribute::String("[1,\"a\"]".to_string()))
        );
        assert_eq!(
            Attribute::from_json(json!({"a": 1})),
            Some(Attribute::String("{\"a\":1}".to_string()))
        );
        assert_eq!(
            serde_json::to_value(Attribute::Integer(7)).unwrap(),
            json!({"type": "integer", "value": 7})
        );
    }

    #[test]
    fn encodes_a_log_envelope() {
        let envelope = Envelope::logs(vec![
            record("App Opened", [("os.name", Attribute::from("Linux"))]),
            record("Editor Edited", [("duration", Attribute::Integer(12))]),
        ]);
        let sent_at = DateTime::parse_from_rfc3339("2026-10-10T12:00:00.250Z")
            .unwrap()
            .with_timezone(&Utc);
        let bytes = envelope.to_bytes(sent_at).unwrap();
        let lines = split_envelope(&bytes);
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0],
            json!({"event_id": envelope.event_id(), "sent_at": "2026-10-10T12:00:00.250Z"})
        );
        let payload = serde_json::to_vec(&lines[2]).unwrap();
        assert_eq!(
            lines[1],
            json!({
                "type": "log",
                "length": payload.len(),
                "item_count": 2,
                "content_type": "application/vnd.sentry.items.log+json",
            })
        );
        assert_eq!(
            lines[2],
            json!({"items": [
                {
                    "timestamp": 1_700_000_000.5,
                    "trace_id": "0".repeat(32),
                    "level": "info",
                    "body": "App Opened",
                    "attributes": {"os.name": {"type": "string", "value": "Linux"}},
                },
                {
                    "timestamp": 1_700_000_000.5,
                    "trace_id": "0".repeat(32),
                    "level": "info",
                    "body": "Editor Edited",
                    "attributes": {"duration": {"type": "integer", "value": 12}},
                },
            ]})
        );
    }

    #[test]
    fn encodes_an_event_envelope() {
        let mut event = SentryEvent::new(EventLevel::Warning, "Hang: draw");
        event.fingerprint = vec!["late_frame".to_string(), "draw".to_string()];
        let envelope = Envelope::event(event.clone());
        let lines = split_envelope(&envelope.to_bytes(Utc::now()).unwrap());
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["event_id"], json!(event.event_id));
        assert_eq!(lines[1]["type"], json!("event"));
        assert_eq!(lines[2]["level"], json!("warning"));
        assert_eq!(lines[2]["platform"], json!("native"));
        assert_eq!(lines[2]["logentry"], json!({"formatted": "Hang: draw"}));
        assert_eq!(lines[2]["fingerprint"], json!(["late_frame", "draw"]));
        assert_eq!(lines[2].get("tags"), None);
    }

    #[test]
    fn encodes_feedback_with_an_attachment() {
        let message = "x".repeat(MAX_FEEDBACK_MESSAGE_CHARS + 10);
        let event = SentryEvent::feedback(&message);
        let envelope = Envelope::feedback(
            event.clone(),
            Some(Attachment {
                filename: "feedback.json".to_string(),
                content_type: "application/json",
                data: br#"{"thread":[]}"#.to_vec(),
            }),
        );
        let bytes = envelope.to_bytes(Utc::now()).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        let lines: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
        assert_eq!(lines.len(), 5);
        let feedback_header: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(feedback_header["type"], json!("feedback"));
        let feedback: Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(feedback["event_id"], json!(event.event_id));
        assert_eq!(feedback.get("logentry"), None);
        assert_eq!(
            feedback["contexts"]["feedback"]["message"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            MAX_FEEDBACK_MESSAGE_CHARS
        );
        let attachment_header: Value = serde_json::from_str(lines[3]).unwrap();
        assert_eq!(
            attachment_header,
            json!({
                "type": "attachment",
                "length": 13,
                "filename": "feedback.json",
                "content_type": "application/json",
            })
        );
        assert_eq!(lines[4], r#"{"thread":[]}"#);
    }

    #[test]
    fn splits_logs_by_count() {
        let records = (0..MAX_LOGS_PER_ITEM + 1)
            .map(|index| record(&format!("event {index}"), []))
            .collect();
        let envelopes = log_envelopes(records).unwrap();
        let counts: Vec<usize> = envelopes
            .iter()
            .map(|envelope| match &envelope.items[..] {
                [Item::Logs(records)] => records.len(),
                items => panic!("unexpected items {items:?}"),
            })
            .collect();
        assert_eq!(counts, [MAX_LOGS_PER_ITEM, 1]);
    }

    #[test]
    fn splits_logs_by_size_and_shrinks_oversized_attributes() {
        let large = "a".repeat(MAX_ITEM_BYTES / 2);
        let oversized = "b".repeat(2 * MAX_ITEM_BYTES);
        let envelopes = log_envelopes(vec![
            record("first", [("payload", Attribute::from(large.as_str()))]),
            record("second", [("payload", Attribute::from(large.as_str()))]),
            record(
                "third",
                [
                    ("sample_data", Attribute::from(oversized.as_str())),
                    ("kept", Attribute::from("small")),
                ],
            ),
        ])
        .unwrap();
        assert_eq!(envelopes.len(), 3);
        for envelope in &envelopes {
            let bytes = envelope.to_bytes(Utc::now()).unwrap();
            let lines = split_envelope(&bytes);
            let length = lines[1]["length"].as_u64().unwrap();
            assert!(
                length <= MAX_ITEM_BYTES as u64,
                "log item is {length} bytes"
            );
        }
        let [Item::Logs(third)] = &envelopes[2].items[..] else {
            panic!("expected one log item");
        };
        assert_eq!(
            third[0].attributes,
            BTreeMap::from([
                ("kept".to_string(), Attribute::from("small")),
                (
                    "sample_data.bytes".to_string(),
                    Attribute::Integer(2 * MAX_ITEM_BYTES as i64)
                ),
            ])
        );
    }

    #[test]
    fn sends_envelopes_to_the_envelope_endpoint() {
        let (http, requests) = recording_http_client();
        let envelope = Envelope::logs(vec![record("App Opened", [])]);
        smol::block_on(send_envelope(&*http, &dsn(), &envelope)).unwrap();

        let requests = requests.lock();
        let [request] = &requests[..] else {
            panic!("expected one request, got {}", requests.len());
        };
        assert_eq!(request.uri, dsn().envelope_url());
        assert_eq!(
            request.content_type.as_deref(),
            Some("application/x-sentry-envelope")
        );
        assert_eq!(
            request.envelope_lines()[2]["items"][0]["body"],
            json!("App Opened")
        );
    }

    #[test]
    fn reports_rejected_envelopes() {
        let http = FakeHttpClient::create(|_| async move {
            Ok(Response::builder()
                .status(429)
                .body(AsyncBody::from("rate limited".to_string()))?)
        });
        let envelope = Envelope::logs(vec![record("App Opened", [])]);
        let error = smol::block_on(send_envelope(&*http, &dsn(), &envelope))
            .unwrap_err()
            .to_string();
        assert!(error.contains("429"), "{error}");
        assert!(error.contains("rate limited"), "{error}");
    }
}
