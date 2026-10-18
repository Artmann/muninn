//! Inbound webhooks: a third party POSTs to a secret URL and the
//! webhook's settings turn the body into a datapoint.

pub mod http;

mod model;
mod rate;
mod service;

pub use model::*;
pub use service::*;
