//! rustinx: a Rust manager that supervises Nginx.

pub mod acme;
pub mod config;
pub mod logging;
pub mod nginx;
pub mod signals;
pub mod storage;

pub use acme::{cert_store, certificates};
pub use nginx::{health, https, supervisor};
