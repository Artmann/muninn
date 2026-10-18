use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

/// A search hit with what selection needs to know about it.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: Uuid,
    pub score: f32,
    pub timestamp: DateTime<Utc>,
    pub content_tokens: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sort {
    #[default]
    Relevance,
    Chronological,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Level {
    #[default]
    Low,
    Medium,
    High,
    ExtraHigh,
}

impl Level {
    pub fn tokens(&self) -> usize {
        match self {
            Level::Low => 8000,
            Level::Medium => 16000,
            Level::High => 24000,
            Level::ExtraHigh => 32000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SelectParams {
    pub limit: usize,
    pub budget_tokens: usize,
    pub query_timestamp: DateTime<Utc>,
    pub occurred_from: Option<DateTime<Utc>>,
    pub occurred_to: Option<DateTime<Utc>>,
    pub sort: Sort,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    pub rank: usize,
    pub id: Uuid,
    pub score: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Selected {
    pub items: Vec<Selection>,
    pub used_tokens: usize,
}

/// Pick what to return: best score first, inside the time window, as many
/// as fit the token budget (skipping ones that don't fit), up to `limit`.
pub fn select(mut candidates: Vec<Candidate>, p: &SelectParams) -> Selected {
    candidates.sort_by(|a, b| b.score.total_cmp(&a.score));

    let in_window = |c: &Candidate| {
        c.timestamp <= p.query_timestamp
            && p.occurred_from.is_none_or(|from| c.timestamp >= from)
            && p.occurred_to.is_none_or(|to| c.timestamp <= to)
    };

    let mut kept: Vec<(Selection, DateTime<Utc>)> = Vec::new();
    let mut used_tokens = 0;
    for c in candidates.into_iter().filter(in_window) {
        if kept.len() >= p.limit {
            break;
        }
        if used_tokens + c.content_tokens > p.budget_tokens {
            continue;
        }
        used_tokens += c.content_tokens;
        kept.push((
            Selection {
                rank: kept.len() + 1,
                id: c.id,
                score: c.score,
            },
            c.timestamp,
        ));
    }

    if p.sort == Sort::Chronological {
        kept.sort_by(|(a, at), (b, bt)| at.cmp(bt).then(a.id.cmp(&b.id)));
    }

    Selected {
        items: kept.into_iter().map(|(s, _)| s).collect(),
        used_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + secs, 0).unwrap()
    }

    fn c(score: f32, ts_secs: i64, tokens: usize) -> Candidate {
        Candidate {
            id: Uuid::now_v7(),
            score,
            timestamp: at(ts_secs),
            content_tokens: tokens,
        }
    }

    fn params() -> SelectParams {
        SelectParams {
            limit: 100,
            budget_tokens: 8000,
            query_timestamp: at(1_000),
            occurred_from: None,
            occurred_to: None,
            sort: Sort::Relevance,
        }
    }

    fn ids(s: &Selected) -> Vec<Uuid> {
        s.items.iter().map(|i| i.id).collect()
    }

    #[test]
    fn keeps_in_score_order_and_assigns_rank() {
        let cands = vec![c(0.9, 0, 10), c(0.5, 0, 10), c(0.7, 0, 10)];
        let want = vec![cands[0].id, cands[2].id, cands[1].id];
        let s = select(cands, &params());
        assert_eq!(ids(&s), want);
        let ranks: Vec<usize> = s.items.iter().map(|i| i.rank).collect();
        assert_eq!(ranks, vec![1, 2, 3]);
        assert_eq!(s.used_tokens, 30);
    }

    #[test]
    fn budget_skips_big_and_keeps_smaller_later() {
        let cands = vec![c(0.9, 0, 60), c(0.8, 0, 60), c(0.7, 0, 30)];
        let want = vec![cands[0].id, cands[2].id];
        let s = select(
            cands,
            &SelectParams {
                budget_tokens: 100,
                ..params()
            },
        );
        assert_eq!(ids(&s), want);
        assert_eq!(s.used_tokens, 90);
    }

    #[test]
    fn budget_too_small_for_all_gives_empty() {
        let s = select(
            vec![c(0.9, 0, 50), c(0.8, 0, 50)],
            &SelectParams {
                budget_tokens: 10,
                ..params()
            },
        );
        assert!(s.items.is_empty());
        assert_eq!(s.used_tokens, 0);
    }

    #[test]
    fn limit_stops_walk() {
        let cands = (0..5).map(|i| c(i as f32 / 10.0, 0, 1)).collect();
        let s = select(
            cands,
            &SelectParams {
                limit: 2,
                ..params()
            },
        );
        assert_eq!(s.items.len(), 2);
    }

    #[test]
    fn excludes_after_query_timestamp() {
        let cands = vec![c(0.9, 1_001, 1), c(0.8, 1_000, 1), c(0.7, 999, 1)];
        let want = vec![cands[1].id, cands[2].id];
        assert_eq!(ids(&select(cands, &params())), want);
    }

    #[test]
    fn occurred_window_is_inclusive_both_ends() {
        let cands = vec![
            c(0.9, 100, 1),
            c(0.8, 200, 1),
            c(0.7, 99, 1),
            c(0.6, 201, 1),
        ];
        let want = vec![cands[0].id, cands[1].id];
        let s = select(
            cands,
            &SelectParams {
                occurred_from: Some(at(100)),
                occurred_to: Some(at(200)),
                ..params()
            },
        );
        assert_eq!(ids(&s), want);
    }

    #[test]
    fn chronological_keeps_rank() {
        let newer = c(0.9, 500, 1);
        let older = c(0.5, 100, 1);
        let want = vec![older.id, newer.id];
        let s = select(
            vec![newer, older],
            &SelectParams {
                sort: Sort::Chronological,
                ..params()
            },
        );
        assert_eq!(ids(&s), want);
        let ranks: Vec<usize> = s.items.iter().map(|i| i.rank).collect();
        assert_eq!(ranks, vec![2, 1]);
    }

    #[test]
    fn level_tokens() {
        assert_eq!(Level::Low.tokens(), 8000);
        assert_eq!(Level::Medium.tokens(), 16000);
        assert_eq!(Level::High.tokens(), 24000);
        assert_eq!(Level::ExtraHigh.tokens(), 32000);
        assert_eq!(
            serde_json::from_str::<Level>("\"extra-high\"").unwrap(),
            Level::ExtraHigh
        );
        assert_eq!(
            serde_json::from_str::<Sort>("\"chronological\"").unwrap(),
            Sort::Chronological
        );
    }
}
