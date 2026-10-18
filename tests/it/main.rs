//! All integration tests, in one binary so the dependencies are linked
//! once instead of once per suite. Run one suite with a filter, e.g.
//! `cargo test --test it ingest:: -- --ignored`.

mod common;
mod e2e_support;

mod boot;
mod cancel;
mod datapoints;
mod delete;
mod e2e;
mod e2e_model_switch;
mod ingest;
mod real_model;
mod recall;
mod s3vectors;
mod store;
mod webhooks;
