//! review-monitor 核心库：与 GUI / WebView 无关的全部业务逻辑。
pub mod activate;
pub mod config;
pub mod model;
pub mod notify;
pub mod parse;
pub mod poller;
pub mod rules;
pub mod schedule;
pub mod store;

pub use model::{Platform, Review};
