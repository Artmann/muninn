use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// A datapoint as sent by a caller. The server assigns the id.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewDatapoint {
    pub project_id: Uuid,
    pub label: String,
    pub content: String,
    pub timestamp: Option<DateTime<Utc>>,
    pub metadata: Option<Map<String, Value>>,
    pub audience: Option<String>,
}

/// How far ahead of the server clock a `timestamp` may be. Recall only sees
/// datapoints at or before its `queryTimestamp` (default now), so a future
/// datapoint would be silently invisible until then.
pub const MAX_CLOCK_SKEW: chrono::Duration = chrono::Duration::minutes(5);

impl NewDatapoint {
    pub fn validate(&self) -> Result<(), String> {
        self.validate_at(Utc::now())
    }

    pub fn validate_at(&self, now: DateTime<Utc>) -> Result<(), String> {
        if let Some(ts) = self.timestamp
            && ts > now + MAX_CLOCK_SKEW
        {
            return Err(format!(
                "timestamp {} is in the future; at most {} minutes ahead of server time ({}) is allowed",
                ts.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                MAX_CLOCK_SKEW.num_minutes(),
                now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ));
        }
        if self.label.trim().is_empty() {
            return Err("label must not be empty".into());
        }
        if self.content.trim().is_empty() {
            return Err("content must not be empty".into());
        }
        Ok(())
    }

    /// Identifies the request behind an idempotency key, so a retry can be
    /// told apart from a different request reusing the key. Encoding
    /// differences don't count: omitted `metadata` equals `{}`, object keys
    /// are compared sorted, and timestamps as instants. An omitted
    /// `timestamp` stays distinct from an explicit one.
    pub fn fingerprint(&self) -> String {
        #[derive(Serialize)]
        struct Canonical<'a> {
            project_id: Uuid,
            label: &'a str,
            content: &'a str,
            timestamp: Option<DateTime<Utc>>,
            metadata: &'a Map<String, Value>,
            audience: Option<&'a str>,
        }
        let empty = Map::new();
        let canonical = Canonical {
            project_id: self.project_id,
            label: &self.label,
            content: &self.content,
            timestamp: self.timestamp,
            metadata: self.metadata.as_ref().unwrap_or(&empty),
            audience: self.audience.as_deref(),
        };
        let bytes = serde_json::to_vec(&canonical).expect("serialising plain data cannot fail");
        hex::encode(Sha256::digest(bytes))
    }

    pub fn into_parts(self, id: Uuid, now: DateTime<Utc>) -> Datapoint {
        Datapoint {
            id,
            project_id: self.project_id,
            label: self.label,
            content: self.content,
            timestamp: self.timestamp.unwrap_or(now),
            metadata: self.metadata.unwrap_or_default(),
            audience: self.audience,
        }
    }
}

/// A stored datapoint, as returned to callers.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Datapoint {
    pub id: Uuid,
    pub project_id: Uuid,
    pub label: String,
    pub content: String,
    pub timestamp: DateTime<Utc>,
    pub metadata: Map<String, Value>,
    pub audience: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredEmbedding {
    pub model: String,
    pub dim: usize,
    pub vector: Vec<f32>,
}

/// Everything about a datapoint except its content: the `meta.json` object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    pub id: Uuid,
    pub project_id: Uuid,
    pub label: String,
    pub timestamp: DateTime<Utc>,
    pub metadata: Map<String, Value>,
    pub audience: Option<String>,
    pub embedding: StoredEmbedding,
    pub content_tokens: usize,
}

impl Meta {
    pub fn into_datapoint(self, content: String) -> Datapoint {
        Datapoint {
            id: self.id,
            project_id: self.project_id,
            label: self.label,
            content,
            timestamp: self.timestamp,
            metadata: self.metadata,
            audience: self.audience,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    #[test]
    fn future_timestamp_beyond_skew_is_rejected() {
        let now: DateTime<Utc> = "2026-10-18T12:00:00Z".parse().unwrap();
        let mut d = base();
        d.timestamp = Some(now + chrono::Duration::minutes(5));
        assert!(d.validate_at(now).is_ok(), "within allowed skew");
        d.timestamp = Some(now + chrono::Duration::minutes(5) + chrono::Duration::seconds(1));
        let err = d.validate_at(now).unwrap_err();
        assert!(err.contains("future"), "{err}");
        d.timestamp = Some(now - chrono::Duration::days(365));
        assert!(d.validate_at(now).is_ok(), "past is fine");
    }

    fn base() -> NewDatapoint {
        NewDatapoint {
            project_id: Uuid::now_v7(),
            label: "label".into(),
            content: "content".into(),
            timestamp: None,
            metadata: None,
            audience: None,
        }
    }

    #[test]
    fn accepts_minimal() {
        assert!(base().validate().is_ok());
    }

    #[test]
    fn rejects_whitespace_label() {
        let n = NewDatapoint {
            label: "  ".into(),
            ..base()
        };
        assert!(n.validate().is_err());
    }

    #[test]
    fn rejects_whitespace_content() {
        let n = NewDatapoint {
            content: "\n\t".into(),
            ..base()
        };
        assert!(n.validate().is_err());
    }

    #[test]
    fn defaults_timestamp_and_metadata() {
        let now = Utc::now();
        let id = Uuid::now_v7();
        let d = base().into_parts(id, now);
        assert_eq!(d.id, id);
        assert_eq!(d.timestamp, now);
        assert!(d.metadata.is_empty());
        assert_eq!(d.audience, None);
    }

    #[test]
    fn offset_timestamp_normalises_to_utc() {
        let n: NewDatapoint = serde_json::from_str(
            r#"{"projectId":"019a0000-0000-7000-8000-000000000000","label":"a","content":"b","timestamp":"2026-10-17T14:00:00+02:00"}"#,
        )
        .unwrap();
        let d = n.into_parts(Uuid::now_v7(), Utc::now());
        assert_eq!(
            serde_json::to_value(&d).unwrap()["timestamp"],
            "2026-10-17T12:00:00Z"
        );
    }

    fn parse(json: &str) -> NewDatapoint {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn fingerprint_ignores_encoding_differences() {
        let p = r#""projectId":"019a0000-0000-7000-8000-000000000000","label":"a","content":"b""#;
        let fp = |rest: &str| parse(&format!("{{{p}{rest}}}")).fingerprint();
        assert_eq!(fp(""), fp(r#","metadata":{}"#), "omitted metadata is {{}}");
        assert_eq!(
            fp(r#","metadata":{"x":1,"y":{"a":1,"b":2}}"#),
            fp(r#","metadata":{"y":{"b":2,"a":1},"x":1}"#),
            "key order"
        );
        assert_eq!(
            fp(r#","timestamp":"2026-10-17T14:00:00+02:00""#),
            fp(r#","timestamp":"2026-10-17T12:00:00Z""#),
            "same instant"
        );
    }

    #[test]
    fn fingerprint_changes_with_any_field() {
        let fp = base().fingerprint();
        let changed = [
            NewDatapoint {
                project_id: Uuid::now_v7(),
                ..base()
            },
            NewDatapoint {
                label: "other".into(),
                ..base()
            },
            NewDatapoint {
                content: "other".into(),
                ..base()
            },
            NewDatapoint {
                timestamp: Some(Utc::now()),
                ..base()
            },
            NewDatapoint {
                metadata: serde_json::from_str(r#"{"k":"v"}"#).unwrap(),
                ..base()
            },
            NewDatapoint {
                audience: Some("eng".into()),
                ..base()
            },
        ];
        for n in changed {
            assert_ne!(n.fingerprint(), fp, "{n:?}");
        }
    }

    #[test]
    fn metadata_must_be_object() {
        let r = serde_json::from_str::<NewDatapoint>(
            r#"{"projectId":"019a0000-0000-7000-8000-000000000000","label":"a","content":"b","metadata":[1]}"#,
        );
        assert!(r.is_err());
    }

    #[test]
    fn wire_names_are_camel_case() {
        let d = base().into_parts(Uuid::now_v7(), Utc::now());
        let v = serde_json::to_value(&d).unwrap();
        assert!(v.get("projectId").is_some());
        assert!(v.get("project_id").is_none());
    }

    #[test]
    fn meta_roundtrip_json() {
        let meta = Meta {
            id: Uuid::now_v7(),
            project_id: Uuid::now_v7(),
            label: "l".into(),
            timestamp: Utc::now(),
            metadata: serde_json::from_str(r#"{"k":"v"}"#).unwrap(),
            audience: Some("eng".into()),
            embedding: StoredEmbedding {
                model: "fake".into(),
                dim: 3,
                vector: vec![0.1, 0.2, 0.3],
            },
            content_tokens: 42,
        };
        let s = serde_json::to_string(&meta).unwrap();
        let back: Meta = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id, meta.id);
        assert_eq!(back.embedding.vector, meta.embedding.vector);
        assert_eq!(back.content_tokens, 42);
        assert_eq!(back.audience.as_deref(), Some("eng"));
        let dp = back.into_datapoint("body".into());
        assert_eq!(dp.content, "body");
        assert_eq!(dp.label, "l");
    }
}
