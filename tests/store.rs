mod common;

use chrono::Utc;
use muninn::domain::{Meta, StoredEmbedding};
use muninn::store::MarkerOutcome;
use uuid::Uuid;

fn meta(project: Uuid, id: Uuid) -> Meta {
    Meta {
        id,
        project_id: project,
        label: "label".into(),
        timestamp: Utc::now(),
        metadata: serde_json::Map::new(),
        audience: None,
        embedding: StoredEmbedding {
            model: "fake".into(),
            dim: 2,
            vector: vec![0.6, 0.8],
        },
        content_tokens: 3,
    }
}

#[tokio::test]
#[ignore]
async fn content_and_meta_roundtrip() {
    let s = common::store().await;
    let (p, id) = (common::project(), Uuid::now_v7());
    s.put_content(p, id, "hello wörld").await.unwrap();
    assert_eq!(
        s.get_content(p, id).await.unwrap().as_deref(),
        Some("hello wörld")
    );
    s.put_meta(&meta(p, id)).await.unwrap();
    let back = s.get_meta(p, id).await.unwrap().unwrap();
    assert_eq!(back.id, id);
    assert_eq!(back.embedding.vector, vec![0.6, 0.8]);
    assert!(s.get_content(p, Uuid::now_v7()).await.unwrap().is_none());
    assert!(s.get_meta(p, Uuid::now_v7()).await.unwrap().is_none());
}

#[tokio::test]
#[ignore]
async fn marker_is_create_only() {
    let s = common::store().await;
    let p = common::project();
    let (first, second) = (Uuid::now_v7(), Uuid::now_v7());
    assert!(matches!(
        s.create_marker(p, "k", first, "fp1").await.unwrap(),
        MarkerOutcome::Created
    ));
    match s.create_marker(p, "k", second, "fp2").await.unwrap() {
        MarkerOutcome::Exists { id, fingerprint } => {
            assert_eq!(id, first);
            assert_eq!(fingerprint.as_deref(), Some("fp1"));
        }
        MarkerOutcome::Created => panic!("second create must not succeed"),
    }
}

#[tokio::test]
#[ignore]
async fn list_metas_finds_all_projects() {
    let s = common::store().await;
    let (p1, p2) = (common::project(), common::project());
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
    s.put_meta(&meta(p1, a)).await.unwrap();
    s.put_meta(&meta(p2, b)).await.unwrap();
    s.put_content(p1, Uuid::now_v7(), "orphan").await.unwrap();
    let mut ids: Vec<Uuid> = s
        .list_metas()
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    ids.sort();
    let mut want = vec![a, b];
    want.sort();
    assert_eq!(ids, want);
}
