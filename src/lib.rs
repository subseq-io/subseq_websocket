//! Reusable websocket server primitives for axum applications.
//!
//! Highlights:
//! - One authenticated websocket route at `GET /ws`.
//! - Typed JSON dispatch via your own enum + `JsonDispatch`.
//! - Raw binary hook for protocol-specific payloads.
//! - SQLx session/connection tracking that merges multiple tabs by `user_id`.

#[cfg(feature = "api")]
pub mod api;

#[cfg(feature = "sqlx")]
pub mod db;

pub mod error;
pub mod models;
pub mod prelude;
