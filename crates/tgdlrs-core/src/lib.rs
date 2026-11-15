//! Core library for Telegram Downloader RS.
//!
//! The crate is deliberately split into small modules so the resumable
//! download state machine can be tested without a Telegram account.

pub mod archive;
pub mod captionfix;
pub mod config;
pub mod database;
pub mod downloader;
pub mod error;
pub mod ipc;
pub mod launch;
pub mod models;
pub mod notify;
pub mod parallel;
pub mod resume;
pub mod sqlite;
pub mod storage;
pub mod telegram;
pub mod upload;
pub mod verification;
pub mod worker;

pub use config::{Config, Credentials, Paths};
pub use error::{Error, Result};
