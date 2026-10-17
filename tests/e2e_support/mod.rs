//! Runs the real `muninn` binary against LocalStack with the real embedding
//! model, and talks to it over HTTP.
#![allow(dead_code)]

use std::fs::File;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use uuid::Uuid;

const READY_TIMEOUT: Duration = Duration::from_secs(120);

pub struct Server {
    child: Child,
    pub base: String,
    pub bucket: String,
    log: PathBuf,
    http: reqwest::Client,
}

impl Server {
    /// A server on a fresh bucket with the default model.
    pub async fn start() -> Server {
        Server::start_with(&format!("muninn-e2e-{}", Uuid::now_v7()), None).await
    }

    /// A server on `bucket`, optionally with another `MUNINN_EMBED_MODEL`.
    /// Waits until `/healthz` reports `ok`.
    pub async fn start_with(bucket: &str, model: Option<&str>) -> Server {
        assert!(
            std::env::var("AWS_REGION").is_ok(),
            "e2e tests need the LocalStack env: set -a; . ./.env.example; set +a"
        );
        let port = TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("free port")
            .port();
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let logs = manifest.join("target").join("e2e-logs");
        std::fs::create_dir_all(&logs).expect("log dir");
        let log = logs.join(format!("{bucket}-{port}.log"));
        let out = File::create(&log).expect("log file");

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_muninn"));
        cmd.env("MUNINN_BIND", format!("127.0.0.1:{port}"))
            .env("MUNINN_BUCKET", bucket)
            .env("MUNINN_MODEL_CACHE", manifest.join(".fastembed_cache"))
            .env("RUST_LOG", "info")
            // Outside the repo so a developer's .env is not picked up.
            .current_dir(std::env::temp_dir())
            .stdout(Stdio::from(out.try_clone().expect("log handle")))
            .stderr(Stdio::from(out));
        if let Some(model) = model {
            cmd.env("MUNINN_EMBED_MODEL", model);
        }
        let mut server = Server {
            child: cmd.spawn().expect("spawn muninn"),
            base: format!("http://127.0.0.1:{port}"),
            bucket: bucket.to_string(),
            log,
            http: reqwest::Client::new(),
        };
        server.wait_ready().await;
        server
    }

    async fn wait_ready(&mut self) {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().expect("child status") {
                panic!("muninn exited with {status}\n{}", self.log_tail());
            }
            if let Ok(resp) = self.http.get(format!("{}/healthz", self.base)).send().await
                && let Ok(body) = resp.json::<Value>().await
                && body["status"] == "ok"
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "muninn not ready after {READY_TIMEOUT:?}\n{}",
                self.log_tail()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub fn log_tail(&self) -> String {
        let text = std::fs::read_to_string(&self.log).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(30);
        format!(
            "--- {} ---\n{}",
            self.log.display(),
            lines[start..].join("\n")
        )
    }

    pub async fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .json(body)
            .send()
            .await
            .expect("request");
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    /// Ingest and expect `201`.
    pub async fn ingest(&self, body: Value) -> Value {
        let (status, resp) = self.post("/api/v1/ingest", &body).await;
        assert_eq!(status, 201, "ingest {body} -> {resp}");
        resp
    }

    /// Recall and expect `200`.
    pub async fn recall(&self, body: Value) -> Value {
        let (status, resp) = self.post("/api/v1/recall", &body).await;
        assert_eq!(status, 200, "recall {body} -> {resp}");
        resp
    }

    /// Kill the process; the bucket stays for a restart.
    pub fn stop(mut self) {
        self.kill();
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.kill();
    }
}

pub fn labels(recall: &Value) -> Vec<String> {
    recall["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|r| r["label"].as_str().expect("label").to_string())
        .collect()
}
