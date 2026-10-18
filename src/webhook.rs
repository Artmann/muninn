//! Inbound webhooks: a third party POSTs to a secret URL and the
//! webhook's settings turn the body into a datapoint.

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::{NewDatapoint, validate_audience};

/// Most webhooks one project holds.
pub const MAX_PER_PROJECT: usize = 20;
/// Most filters on one webhook.
pub const MAX_FILTERS: usize = 20;
/// Most values in one `is-any-of` filter.
pub const MAX_FILTER_VALUES: usize = 50;
/// Longest JSON path accepted.
pub const MAX_PATH_LEN: usize = 500;
/// Largest event body accepted, in bytes.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Longest id taken from an event.
pub const MAX_EVENT_ID_LEN: usize = 200;

#[derive(Debug, Clone, PartialEq)]
enum Segment {
    Key(String),
    Index(usize),
}

/// A JSON path limited to `$`, `.name`, `['name']` and `[n]` segments.
/// Wildcards, `..`, filters, slices and unions are rejected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct JsonPath {
    text: String,
    segments: Vec<Segment>,
}

impl JsonPath {
    pub fn parse(text: &str) -> Result<JsonPath, String> {
        let bad = |why: &str| Err(format!("JSON path {text:?}: {why}"));
        if text.len() > MAX_PATH_LEN {
            return bad(&format!("longer than {MAX_PATH_LEN} characters"));
        }
        let Some(mut rest) = text.strip_prefix('$') else {
            return bad("must start with $");
        };
        let mut segments = Vec::new();
        while !rest.is_empty() {
            if let Some(after) = rest.strip_prefix('.') {
                let end = after.find(['.', '[']).unwrap_or(after.len());
                let name = &after[..end];
                if name.is_empty() {
                    return bad("empty name after `.`; `..` is not supported");
                }
                if name == "*" {
                    return bad("wildcards are not supported");
                }
                segments.push(Segment::Key(name.to_string()));
                rest = &after[end..];
            } else if let Some(after) = rest.strip_prefix('[') {
                let (segment, after) = if let Some(quoted) = after.strip_prefix('\'') {
                    let Some(end) = quoted.find('\'') else {
                        return bad("unclosed quote");
                    };
                    (Segment::Key(quoted[..end].to_string()), &quoted[end + 1..])
                } else {
                    let end = after.find(']').unwrap_or(after.len());
                    let Ok(index) = after[..end].parse::<usize>() else {
                        return bad("brackets hold a quoted name or an index only");
                    };
                    (Segment::Index(index), &after[end..])
                };
                let Some(after) = after.strip_prefix(']') else {
                    return bad("expected `]`");
                };
                segments.push(segment);
                rest = after;
            } else {
                return bad("segments start with `.` or `[`");
            }
        }
        Ok(JsonPath {
            text: text.to_string(),
            segments,
        })
    }

    /// The value at this path, or `None` when any segment is missing.
    pub fn resolve<'a>(&self, mut value: &'a Value) -> Option<&'a Value> {
        for segment in &self.segments {
            value = match segment {
                Segment::Key(k) => value.as_object()?.get(k)?,
                Segment::Index(i) => value.as_array()?.get(*i)?,
            };
        }
        Some(value)
    }
}

impl std::fmt::Display for JsonPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl TryFrom<String> for JsonPath {
    type Error = String;
    fn try_from(text: String) -> Result<Self, String> {
        JsonPath::parse(&text)
    }
}

impl From<JsonPath> for String {
    fn from(path: JsonPath) -> String {
        path.text
    }
}

/// What a filter checks at its path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Condition {
    /// Equals the value. Objects and arrays never match.
    Is { value: Value },
    /// Equals one of the values.
    IsAnyOf { values: Vec<Value> },
    /// A string containing the value, or an array with an element equal to it.
    Contains { value: Value },
    /// Present and not null.
    Exists,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Filter {
    pub path: JsonPath,
    #[serde(flatten)]
    pub condition: Condition,
}

fn is_scalar(v: &Value) -> bool {
    !(v.is_object() || v.is_array())
}

/// A value as it reads in text: strings bare, anything else as JSON.
fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

impl Filter {
    fn validate(&self) -> Result<(), String> {
        let scalar = |v: &Value| {
            if is_scalar(v) {
                Ok(())
            } else {
                Err(format!(
                    "filter on {}: values must be strings, numbers, booleans or null",
                    self.path
                ))
            }
        };
        match &self.condition {
            Condition::Is { value } | Condition::Contains { value } => scalar(value),
            Condition::IsAnyOf { values } => {
                if !(1..=MAX_FILTER_VALUES).contains(&values.len()) {
                    return Err(format!(
                        "filter on {}: is-any-of takes 1 to {MAX_FILTER_VALUES} values",
                        self.path
                    ));
                }
                values.iter().try_for_each(scalar)
            }
            Condition::Exists => Ok(()),
        }
    }

    pub fn matches(&self, body: &Value) -> bool {
        let Some(found) = self.path.resolve(body) else {
            return false;
        };
        match &self.condition {
            Condition::Is { value } => is_scalar(found) && found == value,
            Condition::IsAnyOf { values } => is_scalar(found) && values.contains(found),
            Condition::Contains { value } => match (found, value) {
                (Value::String(s), Value::String(needle)) => s.contains(needle.as_str()),
                (Value::Array(items), _) => items.contains(value),
                _ => false,
            },
            Condition::Exists => !found.is_null(),
        }
    }

    /// The filter in words, for saying why an event was dropped.
    pub fn describe(&self) -> String {
        let p = &self.path;
        match &self.condition {
            Condition::Is { value } => format!("{p} is {}", text_of(value)),
            Condition::IsAnyOf { values } => {
                let list: Vec<String> = values.iter().map(text_of).collect();
                format!("{p} is any of {}", list.join(", "))
            }
            Condition::Contains { value } => format!("{p} contains {}", text_of(value)),
            Condition::Exists => format!("{p} exists"),
        }
    }
}

/// How a webhook maps events to datapoints. Sent when creating or editing
/// a webhook.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub name: String,
    /// The datapoint content. `$` is the whole body.
    pub content_path: JsonPath,
    /// The datapoint label. Defaults to the webhook's name.
    #[serde(default)]
    pub label_path: Option<JsonPath>,
    /// When it happened: ISO 8601, or Unix seconds or milliseconds.
    /// Defaults to the time the event arrived.
    #[serde(default)]
    pub occurred_at_path: Option<JsonPath>,
    /// The sender's id for the event. The datapoint's `sourceId` is
    /// `{webhookId}:{id}`, so resends replace it and two webhooks never
    /// replace each other's datapoints.
    #[serde(default)]
    pub id_path: Option<JsonPath>,
    #[serde(default)]
    pub audience: Option<String>,
    /// All must match for an event to be ingested.
    #[serde(default)]
    pub filters: Vec<Filter>,
    /// A paused webhook refuses events.
    #[serde(default)]
    pub paused: bool,
}

impl Settings {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() || self.name.chars().count() > 64 {
            return Err("name must be 1 to 64 characters".into());
        }
        if let Some(audience) = &self.audience {
            validate_audience(audience)?;
        }
        if self.filters.len() > MAX_FILTERS {
            return Err(format!("a webhook holds at most {MAX_FILTERS} filters"));
        }
        self.filters.iter().try_for_each(Filter::validate)
    }
}

/// A webhook as stored in `webhooks/{id}.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Webhook {
    pub id: String,
    pub project_id: Uuid,
    pub created_at: DateTime<Utc>,
    /// SHA-256 of the URL token. The token itself is only shown once.
    pub token_sha256: String,
    #[serde(flatten)]
    pub settings: Settings,
}

/// A webhook as returned by the API, without its token.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookView {
    pub id: String,
    pub project_id: Uuid,
    pub created_at: DateTime<Utc>,
    #[serde(flatten)]
    pub settings: Settings,
}

pub fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Whether `id` has the shape of a webhook id, so anything else can be
/// refused before it reaches storage.
pub fn is_webhook_id(id: &str) -> bool {
    id.strip_prefix("whk_")
        .is_some_and(|hex| hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

impl Webhook {
    /// A new webhook and the token for its URL.
    pub fn create(project_id: Uuid, settings: Settings, now: DateTime<Utc>) -> (Webhook, String) {
        let token = Uuid::new_v4().simple().to_string();
        let webhook = Webhook {
            id: format!("whk_{}", Uuid::now_v7().simple()),
            project_id,
            created_at: now,
            token_sha256: token_hash(&token),
            settings,
        };
        (webhook, token)
    }

    pub fn token_matches(&self, token: &str) -> bool {
        token_hash(token) == self.token_sha256
    }

    pub fn view(self) -> WebhookView {
        WebhookView {
            id: self.id,
            project_id: self.project_id,
            created_at: self.created_at,
            settings: self.settings,
        }
    }

    /// Turn an event body into a datapoint, or say why it is dropped. A
    /// body that is not JSON is the content as a whole, unless the webhook
    /// has filters, which need JSON.
    pub fn map_event(&self, body: &[u8]) -> Result<NewDatapoint, String> {
        let s = &self.settings;
        let metadata = Map::from_iter([
            ("source".to_string(), json!("webhook")),
            ("webhookId".to_string(), json!(self.id)),
        ]);
        let mut datapoint = NewDatapoint {
            project_id: self.project_id,
            source_id: None,
            label: Some(s.name.clone()),
            content: String::new(),
            timestamp: None,
            metadata: Some(metadata),
            audience: s.audience.clone(),
        };

        let Ok(json) = serde_json::from_slice::<Value>(body) else {
            if !s.filters.is_empty() {
                return Err("the body is not JSON, so no filter can match".into());
            }
            let Ok(text) = std::str::from_utf8(body) else {
                return Err("the body is neither JSON nor UTF-8 text".into());
            };
            if text.trim().is_empty() {
                return Err("the body is empty".into());
            }
            datapoint.content = text.to_string();
            return Ok(datapoint);
        };

        if let Some(failed) = s.filters.iter().find(|f| !f.matches(&json)) {
            return Err(failed.describe());
        }
        datapoint.content = match s.content_path.resolve(&json) {
            None | Some(Value::Null) => {
                return Err(format!("{} resolved to nothing", s.content_path));
            }
            Some(v) => text_of(v),
        };
        if datapoint.content.trim().is_empty() {
            return Err(format!("{} is empty", s.content_path));
        }
        if let Some(path) = &s.label_path
            && let Some(label) = path.resolve(&json).filter(|v| is_scalar(v) && !v.is_null())
            && !text_of(label).trim().is_empty()
        {
            datapoint.label = Some(text_of(label));
        }
        if let Some(path) = &s.occurred_at_path {
            datapoint.timestamp = path.resolve(&json).and_then(parse_time);
        }
        if let Some(path) = &s.id_path {
            let id = path
                .resolve(&json)
                .filter(|v| v.is_string() || v.is_number())
                .map(text_of)
                .filter(|id| !id.trim().is_empty() && id.chars().count() <= MAX_EVENT_ID_LEN);
            let Some(id) = id else {
                return Err(format!(
                    "{path} resolved to no id of 1 to {MAX_EVENT_ID_LEN} characters"
                ));
            };
            datapoint.source_id = Some(format!("{}:{id}", self.id));
        }
        Ok(datapoint)
    }
}

/// An ISO 8601 string, or a Unix time in seconds or milliseconds.
fn parse_time(v: &Value) -> Option<DateTime<Utc>> {
    match v {
        Value::String(s) => DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.with_timezone(&Utc)),
        Value::Number(n) => {
            let n = n.as_f64()?;
            // Seconds stay below 1e11 until the year 5138.
            let millis = if n.abs() >= 1e11 { n } else { n * 1000.0 };
            Utc.timestamp_millis_opt(millis as i64).single()
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(p: &str) -> JsonPath {
        JsonPath::parse(p).unwrap()
    }

    #[test]
    fn paths_parse_and_resolve() {
        let body = json!({ "a": { "b c": [10, { "d": "x" }] }, "e": null });
        assert_eq!(path("$").resolve(&body), Some(&body));
        assert_eq!(path("$.a['b c'][0]").resolve(&body), Some(&json!(10)));
        assert_eq!(path("$.a['b c'][1].d").resolve(&body), Some(&json!("x")));
        assert_eq!(path("$.e").resolve(&body), Some(&Value::Null));
        assert_eq!(path("$.missing").resolve(&body), None);
        assert_eq!(path("$.a[5]").resolve(&body), None);
        assert_eq!(path("$[0]").resolve(&body), None, "not an array");
    }

    #[test]
    fn unsupported_paths_are_rejected() {
        for bad in [
            "",
            "a.b",
            "$..a",
            "$.*",
            "$[*]",
            "$[1:2]",
            "$[0,1]",
            "$[?(@.a)]",
            "$['a'",
            "$.a b[",
            "$a",
            &format!("$.{}", "x".repeat(MAX_PATH_LEN)),
        ] {
            assert!(JsonPath::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn path_serialises_as_its_text() {
        let p: JsonPath = serde_json::from_value(json!("$.a[0]")).unwrap();
        assert_eq!(serde_json::to_value(&p).unwrap(), json!("$.a[0]"));
        assert!(serde_json::from_value::<JsonPath>(json!("$..a")).is_err());
    }

    fn filter(v: Value) -> Filter {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn filter_operators() {
        let body = json!({ "kind": "issue", "n": 3, "tags": ["bug", "p1"], "title": "Crash on save", "o": {} , "z": null });
        let cases = [
            (
                json!({ "path": "$.kind", "op": "is", "value": "issue" }),
                true,
            ),
            (
                json!({ "path": "$.kind", "op": "is", "value": "Issue" }),
                false,
            ),
            (json!({ "path": "$.n", "op": "is", "value": 3 }), true),
            (json!({ "path": "$.o", "op": "is", "value": "x" }), false),
            (
                json!({ "path": "$.missing", "op": "is", "value": "x" }),
                false,
            ),
            (
                json!({ "path": "$.kind", "op": "is-any-of", "values": ["pr", "issue"] }),
                true,
            ),
            (
                json!({ "path": "$.kind", "op": "is-any-of", "values": ["pr"] }),
                false,
            ),
            (
                json!({ "path": "$.title", "op": "contains", "value": "save" }),
                true,
            ),
            (
                json!({ "path": "$.tags", "op": "contains", "value": "p1" }),
                true,
            ),
            (
                json!({ "path": "$.tags", "op": "contains", "value": "p" }),
                false,
            ),
            (json!({ "path": "$.title", "op": "exists" }), true),
            (json!({ "path": "$.z", "op": "exists" }), false),
            (json!({ "path": "$.missing", "op": "exists" }), false),
        ];
        for (f, want) in cases {
            assert_eq!(filter(f.clone()).matches(&body), want, "{f}");
        }
    }

    #[test]
    fn filter_descriptions() {
        assert_eq!(
            filter(json!({ "path": "$.webhookEvent", "op": "is", "value": "jira:issue_updated" }))
                .describe(),
            "$.webhookEvent is jira:issue_updated"
        );
        assert_eq!(
            filter(json!({ "path": "$.n", "op": "is-any-of", "values": [1, "two"] })).describe(),
            "$.n is any of 1, two"
        );
        assert_eq!(
            filter(json!({ "path": "$.a", "op": "exists" })).describe(),
            "$.a exists"
        );
    }

    fn settings(extra: Value) -> Settings {
        let mut v = json!({ "name": "Jira", "contentPath": "$.issue.description" });
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn settings_validation() {
        assert!(settings(json!({})).validate().is_ok());
        let bad = [
            json!({ "name": "" }),
            json!({ "name": "x".repeat(65) }),
            json!({ "audience": "Not A Slug" }),
            json!({ "filters": [{ "path": "$.a", "op": "is", "value": { "o": 1 } }] }),
            json!({ "filters": [{ "path": "$.a", "op": "is-any-of", "values": [] }] }),
            json!({ "filters": vec![json!({ "path": "$.a", "op": "exists" }); MAX_FILTERS + 1] }),
        ];
        for extra in bad {
            assert!(settings(extra.clone()).validate().is_err(), "{extra}");
        }
    }

    fn webhook(extra: Value) -> Webhook {
        Webhook::create(Uuid::now_v7(), settings(extra), Utc::now()).0
    }

    #[test]
    fn create_gives_id_and_token_that_matches() {
        let (w, token) = Webhook::create(Uuid::now_v7(), settings(json!({})), Utc::now());
        assert!(is_webhook_id(&w.id), "{}", w.id);
        assert_eq!(token.len(), 32);
        assert!(w.token_matches(&token));
        assert!(!w.token_matches("wrong"));
        let stored = serde_json::to_value(&w).unwrap();
        assert!(!stored.to_string().contains(&token), "token is not stored");
        assert!(
            serde_json::to_value(w.view())
                .unwrap()
                .get("tokenSha256")
                .is_none()
        );
    }

    #[test]
    fn maps_event_fields() {
        let w = webhook(json!({
            "labelPath": "$.issue.key",
            "occurredAtPath": "$.timestamp",
            "idPath": "$.issue.key",
            "audience": "support"
        }));
        let body = json!({
            "timestamp": 1_760_000_000_000_i64,
            "issue": { "key": "SUP-1042", "description": "Customer cannot log in" }
        });
        let d = w.map_event(body.to_string().as_bytes()).unwrap();
        assert_eq!(d.project_id, w.project_id);
        assert_eq!(d.content, "Customer cannot log in");
        assert_eq!(d.label.as_deref(), Some("SUP-1042"));
        assert_eq!(d.source_id, Some(format!("{}:SUP-1042", w.id)));
        assert_eq!(d.audience.as_deref(), Some("support"));
        assert_eq!(
            d.timestamp,
            Some(Utc.timestamp_millis_opt(1_760_000_000_000).unwrap())
        );
        assert_eq!(
            d.metadata.unwrap(),
            Map::from_iter([
                ("source".to_string(), json!("webhook")),
                ("webhookId".to_string(), json!(w.id)),
            ])
        );
    }

    #[test]
    fn defaults_when_optional_paths_are_absent() {
        let w = webhook(json!({ "labelPath": "$.nope", "occurredAtPath": "$.nope" }));
        let d = w
            .map_event(
                json!({ "issue": { "description": "hi" } })
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        assert_eq!(d.label.as_deref(), Some("Jira"));
        assert_eq!(d.timestamp, None);
        assert_eq!(d.source_id, None);
    }

    #[test]
    fn non_string_content_is_its_json_text() {
        let w = webhook(json!({ "contentPath": "$" }));
        let d = w.map_event(br#"{"a":1}"#).unwrap();
        assert_eq!(d.content, r#"{"a":1}"#);
    }

    #[test]
    fn non_json_body_is_the_content() {
        let w = webhook(json!({}));
        assert_eq!(w.map_event(b"plain text").unwrap().content, "plain text");
        assert!(w.map_event(b"   ").is_err());
        let filtered = webhook(json!({ "filters": [{ "path": "$.a", "op": "exists" }] }));
        assert!(filtered.map_event(b"plain text").is_err());
    }

    #[test]
    fn drop_reasons() {
        let w = webhook(json!({
            "idPath": "$.id",
            "filters": [{ "path": "$.webhookEvent", "op": "is", "value": "jira:issue_created" }]
        }));
        let event = |v: Value| w.map_event(v.to_string().as_bytes());
        assert_eq!(
            event(json!({ "webhookEvent": "jira:issue_updated" })).unwrap_err(),
            "$.webhookEvent is jira:issue_created"
        );
        let ok = json!({ "webhookEvent": "jira:issue_created", "id": 7, "issue": { "description": "d" } });
        assert_eq!(event(ok).unwrap().source_id, Some(format!("{}:7", w.id)));
        let no_content = json!({ "webhookEvent": "jira:issue_created", "id": 7 });
        assert!(
            event(no_content)
                .unwrap_err()
                .contains("$.issue.description")
        );
        let no_id =
            json!({ "webhookEvent": "jira:issue_created", "issue": { "description": "d" } });
        assert!(event(no_id).unwrap_err().contains("$.id"));
    }

    #[test]
    fn times_in_seconds_millis_and_iso() {
        let want = Utc.timestamp_opt(1_760_000_000, 0).unwrap();
        assert_eq!(parse_time(&json!(1_760_000_000)), Some(want));
        assert_eq!(parse_time(&json!(1_760_000_000_000_i64)), Some(want));
        assert_eq!(parse_time(&json!(want.to_rfc3339())), Some(want));
        assert_eq!(parse_time(&json!("yesterday")), None);
        assert_eq!(parse_time(&json!(true)), None);
    }
}
