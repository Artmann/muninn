use std::path::Path;

use muninn::embed::{Embedder, FastEmbedder, ModelChoice};

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[test]
#[ignore]
fn real_model_embeds_multilingual() {
    let e = FastEmbedder::new(
        ModelChoice::MultilingualE5Small,
        Path::new("./.fastembed_cache"),
    )
    .unwrap();
    assert_eq!(e.dim(), 384);
    let q = e
        .embed_query("where do we keep the deployment notes")
        .unwrap();
    let v = e
        .embed_passages(&[
            "Deploymentanteckningarna ligger i Notion under Infra".into(),
            "Recept på kanelbullar".into(),
        ])
        .unwrap();
    assert!(dot(&q, &v[0]) > dot(&q, &v[1]));
    assert!((q.iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-3);
}
