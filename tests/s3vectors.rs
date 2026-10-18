//! Runs against real Amazon S3 Vectors; LocalStack does not emulate it.
//! Set `TEST_VECTOR_BUCKET` and AWS credentials/region (without
//! `AWS_ENDPOINT_URL`), then `cargo test --test s3vectors -- --ignored`.
//! Creates a throwaway index in that bucket and deletes it afterwards.

use muninn::index::{S3VectorsIndex, VectorIndex, index_name};
use muninn::store::S3Store;
use uuid::Uuid;

#[tokio::test]
#[ignore]
async fn upsert_and_search_scoped_by_project() {
    let Ok(bucket) = std::env::var("TEST_VECTOR_BUCKET") else {
        eprintln!("TEST_VECTOR_BUCKET not set; skipping");
        return;
    };
    let sdk = S3Store::load_sdk().await.unwrap();
    let model = format!("test-{}", Uuid::now_v7().simple());
    let idx = S3VectorsIndex::connect(&sdk, bucket.clone(), &model, 3)
        .await
        .unwrap();

    let result = async {
        let (p1, p2) = (Uuid::now_v7(), Uuid::now_v7());
        let (a, b, c) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        idx.upsert_many(vec![
            (p1, a, vec![1.0, 0.0, 0.0]),
            (p1, b, vec![0.6, 0.8, 0.0]),
            (p2, c, vec![1.0, 0.0, 0.0]),
        ])
        .await?;
        // Overwrite b; the old vector must be gone.
        idx.upsert(p1, b, vec![0.0, 1.0, 0.0]).await?;

        let hits = idx.search(p1, &[1.0, 0.0, 0.0], 10).await?;
        let ids: Vec<Uuid> = hits.iter().map(|h| h.0).collect();
        anyhow::ensure!(ids == vec![a, b], "hits {hits:?}");
        anyhow::ensure!(
            (hits[0].1 - 1.0).abs() < 1e-3,
            "identical scores ~1: {hits:?}"
        );
        anyhow::ensure!(hits[1].1.abs() < 1e-3, "orthogonal scores ~0: {hits:?}");

        let top1 = idx.search(p1, &[1.0, 0.0, 0.0], 1).await?;
        anyhow::ensure!(top1.len() == 1 && top1[0].0 == a, "k caps: {top1:?}");
        anyhow::Ok(())
    }
    .await;

    aws_sdk_s3vectors::Client::new(&sdk)
        .delete_index()
        .vector_bucket_name(&bucket)
        .index_name(index_name(&model))
        .send()
        .await
        .expect("delete test index");
    result.unwrap();
}

#[tokio::test]
#[ignore]
async fn existing_index_with_other_dimension_is_rejected() {
    let Ok(bucket) = std::env::var("TEST_VECTOR_BUCKET") else {
        eprintln!("TEST_VECTOR_BUCKET not set; skipping");
        return;
    };
    let sdk = S3Store::load_sdk().await.unwrap();
    let model = format!("test-{}", Uuid::now_v7().simple());
    S3VectorsIndex::connect(&sdk, bucket.clone(), &model, 3)
        .await
        .unwrap();
    let again = S3VectorsIndex::connect(&sdk, bucket.clone(), &model, 3).await;
    let other = S3VectorsIndex::connect(&sdk, bucket.clone(), &model, 4).await;

    aws_sdk_s3vectors::Client::new(&sdk)
        .delete_index()
        .vector_bucket_name(&bucket)
        .index_name(index_name(&model))
        .send()
        .await
        .expect("delete test index");
    assert!(again.is_ok(), "reconnect to same index: {:?}", again.err());
    let err = format!("{:#}", other.err().expect("dimension mismatch"));
    assert!(err.contains("dimension"), "{err}");
}
