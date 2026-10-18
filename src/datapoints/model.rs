use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// A datapoint as sent by a caller. The server assigns the id, derived from
/// `source_id` when the caller sends one.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewDatapoint {
    pub project_id: Uuid,
    /// The caller's own id for this datapoint. Sending it again replaces
    /// the stored datapoint.
    pub source_id: Option<String>,
    /// Required unless `source_id` names a stored datapoint, whose label is
    /// then kept.
    pub label: Option<String>,
    pub content: String,
    pub timestamp: Option<DateTime<Utc>>,
    pub metadata: Option<Map<String, Value>>,
    pub audience: Option<String>,
}

/// How far ahead of the server clock a `timestamp` may be. Recall only sees
/// datapoints at or before its `queryTimestamp` (default now), so a future
/// datapoint would be silently invisible until then.
pub const MAX_CLOCK_SKEW: chrono::Duration = chrono::Duration::minutes(5);

/// The audience every reader in the project reaches. Stored as no audience.
pub const PROJECT_AUDIENCE: &str = "project";

/// 2 to 40 lowercase letters, digits and dashes, starting and ending with a
/// letter or digit.
pub fn validate_audience(slug: &str) -> Result<(), String> {
    let lower_alnum = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    let valid = (2..=40).contains(&slug.len())
        && slug.chars().all(|c| lower_alnum(c) || c == '-')
        && slug.starts_with(lower_alnum)
        && slug.ends_with(lower_alnum);
    if valid {
        Ok(())
    } else {
        Err(format!(
            "audience {slug:?} must be 2 to 40 lowercase letters, digits and dashes, \
             starting and ending with a letter or digit"
        ))
    }
}

/// The server id for a caller's `sourceId`. The same pair always maps to
/// the same id, so a resend finds the stored datapoint without a lookup.
pub fn source_uuid(project: Uuid, source_id: &str) -> Uuid {
    Uuid::new_v5(&project, source_id.as_bytes())
}

pub fn content_hash(content: &str) -> String {
    hex::encode(Sha256::digest(content.as_bytes()))
}

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
        if self.source_id.as_ref().is_some_and(|s| s.trim().is_empty()) {
            return Err("sourceId must not be empty; omit it instead".into());
        }
        match &self.label {
            Some(label) if label.trim().is_empty() => {
                return Err("label must not be empty".into());
            }
            None if self.source_id.is_none() => return Err("label is required".into()),
            _ => {}
        }
        if self.content.trim().is_empty() {
            return Err("content must not be empty".into());
        }
        if let Some(audience) = &self.audience {
            validate_audience(audience)?;
        }
        Ok(())
    }

    /// The server id: derived from `source_id`, or freshly minted.
    pub fn id(&self) -> Uuid {
        match &self.source_id {
            Some(source) => source_uuid(self.project_id, source),
            None => Uuid::now_v7(),
        }
    }

    /// Identifies the request behind an idempotency key, so a retry can be
    /// told apart from a different request reusing the key. Encoding
    /// differences don't count: omitted `metadata` equals `{}`, omitted
    /// `audience` equals `project`, object keys are compared sorted, and
    /// timestamps as instants. An omitted `timestamp` stays distinct from an
    /// explicit one.
    pub fn fingerprint(&self) -> String {
        #[derive(Serialize)]
        struct Canonical<'a> {
            project_id: Uuid,
            label: Option<&'a str>,
            content: &'a str,
            timestamp: Option<DateTime<Utc>>,
            metadata: &'a Map<String, Value>,
            audience: Option<&'a str>,
        }
        let empty = Map::new();
        let canonical = Canonical {
            project_id: self.project_id,
            label: self.label.as_deref(),
            content: &self.content,
            timestamp: self.timestamp,
            metadata: self.metadata.as_ref().unwrap_or(&empty),
            audience: self.audience.as_deref().filter(|a| *a != PROJECT_AUDIENCE),
        };
        let bytes = serde_json::to_vec(&canonical).expect("serialising plain data cannot fail");
        hex::encode(Sha256::digest(bytes))
    }

    /// The datapoint to store under `id`. Fields left out keep their value
    /// from `existing`, the stored datapoint this one replaces, or get
    /// defaults: `timestamp` now, `metadata` empty, `audience` the whole
    /// project. Fails when neither has a label.
    pub fn into_parts(
        self,
        id: Uuid,
        existing: Option<&Meta>,
        now: DateTime<Utc>,
    ) -> Result<Datapoint, String> {
        let Some(label) = self.label.or_else(|| existing.map(|m| m.label.clone())) else {
            return Err(format!(
                "label is required: no datapoint with sourceId {:?} is stored",
                self.source_id.unwrap_or_default()
            ));
        };
        let audience = match self.audience {
            None => existing.and_then(|m| m.audience.clone()),
            Some(a) if a == PROJECT_AUDIENCE => None,
            Some(a) => Some(a),
        };
        Ok(Datapoint {
            id,
            project_id: self.project_id,
            source_id: self.source_id,
            label,
            content: self.content,
            timestamp: self
                .timestamp
                .or_else(|| existing.map(|m| m.timestamp))
                .unwrap_or(now),
            metadata: self
                .metadata
                .or_else(|| existing.map(|m| m.metadata.clone()))
                .unwrap_or_default(),
            audience,
        })
    }
}

/// A stored datapoint, as returned to callers.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Datapoint {
    pub id: Uuid,
    pub project_id: Uuid,
    pub source_id: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    pub label: String,
    pub timestamp: DateTime<Utc>,
    pub metadata: Map<String, Value>,
    pub audience: Option<String>,
    pub embedding: StoredEmbedding,
    pub content_tokens: usize,
    /// SHA-256 of the content this meta was computed from, so a resend can
    /// tell whether the content changed without downloading it. Missing on
    /// datapoints written before it was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_sha256: Option<String>,
}

impl Meta {
    /// Whether storing `datapoint` with `model` would change nothing.
    pub fn matches(&self, datapoint: &Datapoint, model: &str) -> bool {
        self.content_sha256.as_deref() == Some(content_hash(&datapoint.content).as_str())
            && self.embedding.model == model
            && self.label == datapoint.label
            && self.timestamp == datapoint.timestamp
            && self.metadata == datapoint.metadata
            && self.audience == datapoint.audience
    }

    pub fn into_datapoint(self, content: String) -> Datapoint {
        Datapoint {
            id: self.id,
            project_id: self.project_id,
            source_id: self.source_id,
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
            source_id: None,
            label: Some("label".into()),
            content: "content".into(),
            timestamp: None,
            metadata: None,
            audience: None,
        }
    }

    fn stored(dp: &Datapoint) -> Meta {
        Meta {
            id: dp.id,
            project_id: dp.project_id,
            source_id: dp.source_id.clone(),
            label: dp.label.clone(),
            timestamp: dp.timestamp,
            metadata: dp.metadata.clone(),
            audience: dp.audience.clone(),
            embedding: StoredEmbedding {
                model: "fake".into(),
                dim: 1,
                vector: vec![1.0],
            },
            content_tokens: 1,
            content_sha256: Some(content_hash(&dp.content)),
        }
    }

    #[test]
    fn accepts_minimal() {
        assert!(base().validate().is_ok());
    }

    #[test]
    fn rejects_whitespace_label() {
        let n = NewDatapoint {
            label: Some("  ".into()),
            ..base()
        };
        assert!(n.validate().is_err());
    }

    #[test]
    fn label_is_optional_only_with_source_id() {
        let n = NewDatapoint {
            label: None,
            ..base()
        };
        assert!(n.validate().unwrap_err().contains("label"));
        let n = NewDatapoint {
            label: None,
            source_id: Some("crm-1".into()),
            ..base()
        };
        assert!(n.validate().is_ok());
    }

    #[test]
    fn rejects_blank_source_id() {
        let n = NewDatapoint {
            source_id: Some(" ".into()),
            ..base()
        };
        assert!(n.validate().unwrap_err().contains("sourceId"));
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
    fn audience_slugs() {
        for ok in ["project", "eng", "a1", "sales-emea", &"x".repeat(40)] {
            assert!(validate_audience(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "a",
            "Eng",
            "-eng",
            "eng-",
            "en g",
            "eng_x",
            &"x".repeat(41),
        ] {
            assert!(validate_audience(bad).is_err(), "{bad}");
        }
        let n = NewDatapoint {
            audience: Some("Engineering".into()),
            ..base()
        };
        assert!(n.validate().unwrap_err().contains("audience"));
    }

    #[test]
    fn source_id_gives_stable_id_per_project() {
        let p = Uuid::now_v7();
        let with = |p, s: &str| {
            NewDatapoint {
                project_id: p,
                source_id: Some(s.into()),
                ..base()
            }
            .id()
        };
        assert_eq!(with(p, "a"), with(p, "a"));
        assert_ne!(with(p, "a"), with(p, "b"));
        assert_ne!(with(p, "a"), with(Uuid::now_v7(), "a"));
        assert_eq!(base().id().get_version_num(), 7);
    }

    #[test]
    fn defaults_timestamp_and_metadata() {
        let now = Utc::now();
        let id = Uuid::now_v7();
        let d = base().into_parts(id, None, now).unwrap();
        assert_eq!(d.id, id);
        assert_eq!(d.timestamp, now);
        assert!(d.metadata.is_empty());
        assert_eq!(d.audience, None);
        assert_eq!(d.source_id, None);
    }

    #[test]
    fn omitted_fields_keep_stored_values() {
        let then: DateTime<Utc> = "2026-01-01T00:00:00Z".parse().unwrap();
        let old = NewDatapoint {
            source_id: Some("s".into()),
            timestamp: Some(then),
            metadata: serde_json::from_str(r#"{"k":"v"}"#).unwrap(),
            audience: Some("eng".into()),
            ..base()
        }
        .into_parts(Uuid::now_v7(), None, Utc::now())
        .unwrap();
        let meta = stored(&old);
        let resend = NewDatapoint {
            project_id: old.project_id,
            source_id: Some("s".into()),
            label: None,
            content: "new content".into(),
            timestamp: None,
            metadata: None,
            audience: None,
        };
        let d = resend.into_parts(old.id, Some(&meta), Utc::now()).unwrap();
        assert_eq!(d.label, "label");
        assert_eq!(d.timestamp, then);
        assert_eq!(d.metadata, old.metadata);
        assert_eq!(d.audience.as_deref(), Some("eng"));
        assert_eq!(d.content, "new content");
    }

    #[test]
    fn project_audience_clears_stored_audience() {
        let old = NewDatapoint {
            audience: Some("eng".into()),
            ..base()
        }
        .into_parts(Uuid::now_v7(), None, Utc::now())
        .unwrap();
        let d = NewDatapoint {
            audience: Some("project".into()),
            ..base()
        }
        .into_parts(old.id, Some(&stored(&old)), Utc::now())
        .unwrap();
        assert_eq!(d.audience, None);
    }

    #[test]
    fn missing_label_without_stored_datapoint_fails() {
        let n = NewDatapoint {
            label: None,
            source_id: Some("crm-1".into()),
            ..base()
        };
        let err = n.into_parts(Uuid::now_v7(), None, Utc::now()).unwrap_err();
        assert!(err.contains("crm-1"), "{err}");
    }

    #[test]
    fn meta_matches_only_identical_datapoint() {
        let d = base().into_parts(Uuid::now_v7(), None, Utc::now()).unwrap();
        let meta = stored(&d);
        assert!(meta.matches(&d, "fake"));
        assert!(!meta.matches(&d, "other-model"));
        let mut changed = d.clone();
        changed.content = "other".into();
        assert!(!meta.matches(&changed, "fake"));
        let mut changed = d.clone();
        changed.audience = Some("eng".into());
        assert!(!meta.matches(&changed, "fake"));
        let legacy = Meta {
            content_sha256: None,
            ..meta
        };
        assert!(!legacy.matches(&d, "fake"), "no hash means unknown content");
    }

    #[test]
    fn offset_timestamp_normalises_to_utc() {
        let n: NewDatapoint = serde_json::from_str(
            r#"{"projectId":"019a0000-0000-7000-8000-000000000000","label":"a","content":"b","timestamp":"2026-10-17T14:00:00+02:00"}"#,
        )
        .unwrap();
        let d = n.into_parts(Uuid::now_v7(), None, Utc::now()).unwrap();
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
            fp(""),
            fp(r#","audience":"project""#),
            "omitted audience is project"
        );
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
    fn fingerprint_is_unchanged_for_existing_markers() {
        // Markers stored before label became optional must still match.
        let n = parse(
            r#"{"projectId":"019a0000-0000-7000-8000-000000000000","label":"a","content":"b"}"#,
        );
        assert_eq!(
            n.fingerprint(),
            "dbee3c3fa62d50e91a84223bade47423470b7a9490c893f8bfd7381f42e034f3"
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
                label: Some("other".into()),
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
        let d = NewDatapoint {
            source_id: Some("s".into()),
            ..base()
        }
        .into_parts(Uuid::now_v7(), None, Utc::now())
        .unwrap();
        let v = serde_json::to_value(&d).unwrap();
        assert!(v.get("projectId").is_some());
        assert!(v.get("project_id").is_none());
        assert_eq!(v["sourceId"], "s");
    }

    #[test]
    fn meta_roundtrip_json() {
        let meta = Meta {
            id: Uuid::now_v7(),
            project_id: Uuid::now_v7(),
            source_id: Some("crm-1".into()),
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
            content_sha256: Some(content_hash("body")),
        };
        let s = serde_json::to_string(&meta).unwrap();
        let back: Meta = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id, meta.id);
        assert_eq!(back.embedding.vector, meta.embedding.vector);
        assert_eq!(back.content_tokens, 42);
        assert_eq!(back.audience.as_deref(), Some("eng"));
        assert_eq!(back.content_sha256, meta.content_sha256);
        let dp = back.into_datapoint("body".into());
        assert_eq!(dp.content, "body");
        assert_eq!(dp.label, "l");
        assert_eq!(dp.source_id.as_deref(), Some("crm-1"));
    }

    #[test]
    fn meta_without_new_fields_still_parses() {
        let old = r#"{"id":"019a0000-0000-7000-8000-000000000000","projectId":"019a0000-0000-7000-8000-000000000001","label":"l","timestamp":"2026-10-17T12:00:00Z","metadata":{},"audience":null,"embedding":{"model":"m","dim":1,"vector":[1.0]},"contentTokens":1}"#;
        let meta: Meta = serde_json::from_str(old).unwrap();
        assert_eq!(meta.source_id, None);
        assert_eq!(meta.content_sha256, None);
    }
}
