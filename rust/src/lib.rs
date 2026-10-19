//! Rust port of aimap, migrated leaf-first from `src/aimap`.
//!
//! Each module here mirrors a Python module of the same name and is pinned to
//! the same behavior by tests (`tests/`): unit ports, golden fixtures, real
//! Postgres integration, and a production S3/mail diff. See `rust/README.md`.

pub mod accounts;
pub mod catalog;
pub mod classifier;
pub mod config;
pub mod crypto;
pub mod db;
pub mod draft;
pub mod home;
pub mod inbox;
pub mod ingest;
pub mod jev;
pub mod parse;
pub mod pgtime;
pub mod profiles;
pub mod queue;
pub mod s3;
pub mod storage;
