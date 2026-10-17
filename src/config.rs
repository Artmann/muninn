use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context;

use crate::embed::ModelChoice;

#[derive(Debug, Clone)]
pub struct Config {
    pub bucket: String,
    pub bind: SocketAddr,
    pub embed_model: ModelChoice,
    pub model_cache: PathBuf,
    pub endpoint_url: Option<String>,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Config> {
        Config::from_map(&std::env::vars().collect())
    }

    pub fn from_map(m: &HashMap<String, String>) -> anyhow::Result<Config> {
        let get = |k: &str| m.get(k).map(String::as_str);
        let bind_raw = get("MUNINN_BIND").unwrap_or("0.0.0.0:8080");
        let bind = bind_raw
            .parse()
            .with_context(|| format!("MUNINN_BIND is not a socket address: {bind_raw}"))?;
        Ok(Config {
            bucket: get("MUNINN_BUCKET").unwrap_or("muninn").to_string(),
            bind,
            embed_model: get("MUNINN_EMBED_MODEL")
                .unwrap_or("multilingual-e5-small")
                .parse()
                .context("MUNINN_EMBED_MODEL")?,
            model_cache: PathBuf::from(get("MUNINN_MODEL_CACHE").unwrap_or("./.fastembed_cache")),
            endpoint_url: get("AWS_ENDPOINT_URL").map(str::to_string),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    #[test]
    fn defaults_apply_when_env_unset() {
        let c = Config::from_map(&HashMap::new()).unwrap();
        assert_eq!(c.bucket, "muninn");
        assert_eq!(c.bind.to_string(), "0.0.0.0:8080");
        assert_eq!(c.embed_model.name(), "multilingual-e5-small");
        assert_eq!(c.model_cache, PathBuf::from("./.fastembed_cache"));
        assert_eq!(c.endpoint_url, None);
    }

    #[test]
    fn env_overrides() {
        let m = HashMap::from([
            ("MUNINN_BUCKET".into(), "b".into()),
            ("MUNINN_BIND".into(), "127.0.0.1:9000".into()),
            ("AWS_ENDPOINT_URL".into(), "http://localhost:4566".into()),
        ]);
        let c = Config::from_map(&m).unwrap();
        assert_eq!(c.bucket, "b");
        assert_eq!(c.bind.port(), 9000);
        assert_eq!(c.endpoint_url.as_deref(), Some("http://localhost:4566"));
    }

    #[test]
    fn unknown_model_is_error() {
        let m = HashMap::from([("MUNINN_EMBED_MODEL".into(), "nope".into())]);
        assert!(Config::from_map(&m).is_err());
    }

    #[test]
    fn bad_bind_is_error() {
        assert!(Config::from_map(&HashMap::from([("MUNINN_BIND".into(), "nope".into())])).is_err());
    }
}
