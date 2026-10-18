use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::str::FromStr;
use std::sync::Mutex;

use anyhow::Context;
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use serde_json::{Map, Value};

/// The embedding models Muninn can run, keyed by their `EMBED_MODEL` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelChoice {
    MultilingualE5Small,
    MultilingualE5Base,
    BgeM3,
    BgeSmallEnV15,
}

impl ModelChoice {
    pub const ALL: [ModelChoice; 4] = [
        ModelChoice::MultilingualE5Small,
        ModelChoice::MultilingualE5Base,
        ModelChoice::BgeM3,
        ModelChoice::BgeSmallEnV15,
    ];

    pub fn name(self) -> &'static str {
        match self {
            ModelChoice::MultilingualE5Small => "multilingual-e5-small",
            ModelChoice::MultilingualE5Base => "multilingual-e5-base",
            ModelChoice::BgeM3 => "bge-m3",
            ModelChoice::BgeSmallEnV15 => "bge-small-en-v1.5",
        }
    }

    pub fn dim(self) -> usize {
        match self {
            ModelChoice::MultilingualE5Small | ModelChoice::BgeSmallEnV15 => 384,
            ModelChoice::MultilingualE5Base => 768,
            ModelChoice::BgeM3 => 1024,
        }
    }

    pub fn fastembed_model(self) -> EmbeddingModel {
        match self {
            ModelChoice::MultilingualE5Small => EmbeddingModel::MultilingualE5Small,
            ModelChoice::MultilingualE5Base => EmbeddingModel::MultilingualE5Base,
            ModelChoice::BgeM3 => EmbeddingModel::BGEM3,
            ModelChoice::BgeSmallEnV15 => EmbeddingModel::BGESmallENV15,
        }
    }

    fn is_e5(self) -> bool {
        matches!(
            self,
            ModelChoice::MultilingualE5Small | ModelChoice::MultilingualE5Base
        )
    }

    pub fn query_prefix(self) -> &'static str {
        if self.is_e5() { "query: " } else { "" }
    }

    pub fn passage_prefix(self) -> &'static str {
        if self.is_e5() { "passage: " } else { "" }
    }
}

impl FromStr for ModelChoice {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ModelChoice::ALL
            .into_iter()
            .find(|m| m.name() == s)
            .with_context(|| {
                let names: Vec<_> = ModelChoice::ALL.iter().map(|m| m.name()).collect();
                format!(
                    "unknown embedding model {s:?}; expected one of {}",
                    names.join(", ")
                )
            })
    }
}

/// The text embedded for a datapoint: label, content, then metadata as
/// `key: value` lines in key order. Non-string values are compact JSON.
pub fn build_input(label: &str, content: &str, metadata: &Map<String, Value>) -> String {
    let mut out = format!("{label}\n{content}");
    let mut keys: Vec<&String> = metadata.keys().collect();
    keys.sort();
    for k in keys {
        let v = match &metadata[k] {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        out.push('\n');
        out.push_str(k);
        out.push_str(": ");
        out.push_str(&v);
    }
    out
}

/// Scale a vector to unit length. A zero vector is left alone.
pub fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

pub trait Embedder: Send + Sync {
    fn model_name(&self) -> &str;
    fn dim(&self) -> usize;
    fn embed_passages(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>>;
    fn embed_query(&self, text: &str) -> anyhow::Result<Vec<f32>>;
}

/// A real model run locally through fastembed.
pub struct FastEmbedder {
    choice: ModelChoice,
    model: Mutex<TextEmbedding>,
}

impl FastEmbedder {
    pub fn new(choice: ModelChoice, cache_dir: &Path) -> anyhow::Result<FastEmbedder> {
        let opts = TextInitOptions::new(choice.fastembed_model())
            .with_cache_dir(cache_dir.to_path_buf())
            .with_show_download_progress(false);
        let model = TextEmbedding::try_new(opts)
            .with_context(|| format!("loading embedding model {}", choice.name()))?;
        Ok(FastEmbedder {
            choice,
            model: Mutex::new(model),
        })
    }

    fn run(&self, texts: Vec<String>) -> anyhow::Result<Vec<Vec<f32>>> {
        let mut model = self
            .model
            .lock()
            .map_err(|_| anyhow::anyhow!("embedding model lock poisoned"))?;
        let mut vectors = model.embed(texts, None)?;
        for v in &mut vectors {
            normalize(v);
        }
        Ok(vectors)
    }
}

impl Embedder for FastEmbedder {
    fn model_name(&self) -> &str {
        self.choice.name()
    }

    fn dim(&self) -> usize {
        self.choice.dim()
    }

    fn embed_passages(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        let prefix = self.choice.passage_prefix();
        self.run(texts.iter().map(|t| format!("{prefix}{t}")).collect())
    }

    fn embed_query(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let prefix = self.choice.query_prefix();
        self.run(vec![format!("{prefix}{text}")])?
            .pop()
            .context("model returned no vector")
    }
}

/// A deterministic bag-of-words embedder for tests. Texts sharing words
/// score higher; no model files needed.
pub struct FakeEmbedder {
    dim: usize,
}

impl FakeEmbedder {
    pub fn new(dim: usize) -> FakeEmbedder {
        FakeEmbedder { dim }
    }

    fn embed_one(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; self.dim];
        for word in text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
        {
            let mut h = DefaultHasher::new();
            word.to_lowercase().hash(&mut h);
            let hash = h.finish();
            let idx = (hash % self.dim as u64) as usize;
            v[idx] += if hash >> 63 == 0 { 1.0 } else { -1.0 };
        }
        normalize(&mut v);
        if self.dim > 0 && v.iter().all(|x| *x == 0.0) {
            v[0] = 1.0;
        }
        v
    }
}

impl Embedder for FakeEmbedder {
    fn model_name(&self) -> &str {
        "fake"
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed_passages(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.embed_one(t)).collect())
    }

    fn embed_query(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        Ok(self.embed_one(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    fn dot(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn parses_config_names() {
        assert_eq!("bge-m3".parse::<ModelChoice>().unwrap(), ModelChoice::BgeM3);
        assert_eq!(
            "multilingual-e5-small".parse::<ModelChoice>().unwrap(),
            ModelChoice::MultilingualE5Small
        );
        assert_eq!(
            "bge-small-en-v1.5".parse::<ModelChoice>().unwrap(),
            ModelChoice::BgeSmallEnV15
        );
        assert!("nope".parse::<ModelChoice>().is_err());
        assert_eq!(
            ModelChoice::MultilingualE5Base.name(),
            "multilingual-e5-base"
        );
    }

    #[test]
    fn dims_match_spec() {
        assert_eq!(ModelChoice::MultilingualE5Small.dim(), 384);
        assert_eq!(ModelChoice::MultilingualE5Base.dim(), 768);
        assert_eq!(ModelChoice::BgeM3.dim(), 1024);
        assert_eq!(ModelChoice::BgeSmallEnV15.dim(), 384);
    }

    #[test]
    fn build_input_orders_metadata() {
        let md: Map<_, _> = serde_json::from_str(r#"{"z":"last","a":{"n":1},"m":"mid"}"#).unwrap();
        assert_eq!(
            build_input("L", "C", &md),
            "L\nC\na: {\"n\":1}\nm: mid\nz: last"
        );
    }

    #[test]
    fn build_input_no_metadata_has_no_trailing_newline() {
        assert_eq!(build_input("L", "C", &Map::new()), "L\nC");
    }

    #[test]
    fn normalize_makes_unit_length_and_skips_zero() {
        let mut v = vec![3.0, 4.0];
        normalize(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6 && (v[1] - 0.8).abs() < 1e-6);
        let mut z = vec![0.0, 0.0];
        normalize(&mut z);
        assert_eq!(z, vec![0.0, 0.0]);
    }

    #[test]
    fn fake_is_deterministic_and_unit_length() {
        let e = FakeEmbedder::new(16);
        assert_eq!(e.model_name(), "fake");
        assert_eq!(e.dim(), 16);
        let a = e.embed_query("hello world").unwrap();
        let b = e.embed_query("hello world").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!((a.iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn fake_similar_texts_score_higher() {
        let e = FakeEmbedder::new(64);
        let q = e.embed_query("rust memory service").unwrap();
        let near = e
            .embed_passages(&["we built a memory service in rust".into()])
            .unwrap()
            .remove(0);
        let far = e
            .embed_passages(&["banana bread recipe".into()])
            .unwrap()
            .remove(0);
        assert!(dot(&q, &near) > dot(&q, &far));
    }

    #[test]
    fn e5_prefixes() {
        assert_eq!(ModelChoice::MultilingualE5Small.query_prefix(), "query: ");
        assert_eq!(
            ModelChoice::MultilingualE5Base.passage_prefix(),
            "passage: "
        );
        assert_eq!(ModelChoice::BgeM3.passage_prefix(), "");
        assert_eq!(ModelChoice::BgeSmallEnV15.query_prefix(), "");
    }
}
