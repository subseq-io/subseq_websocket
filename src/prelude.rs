//! Convenience re-exports for implementing websocket services.
//!
//! Typical setup:
//! 1. Implement `HasPool`, `HasWsHub`, and `HandlesWebSocketEvents` on app state.
//! 2. Mount `routes::<AppState>()` into your axum router.
//! 3. Optionally insert `AuthenticatedUser` into request extensions
//!    (typically via `subseq_auth` middleware) to allow inbound writes.

#[cfg(feature = "api")]
pub use crate::api::{
    HandlesWebSocketEvents, HasPool, HasWsHub, JsonDispatch, OutboundMessage,
    SessionIngressMessage, SessionManager, WsApp, WsHub, routes,
};

#[cfg(feature = "sqlx")]
pub use crate::db::{
    active_connection_count, active_connection_count_for_session, associate_user_with_session,
    close_connection, create_websocket_tables, get_user_session, open_or_resume_session,
    purge_expired_anonymous_sessions, register_connection, touch_connection,
};

pub use crate::error::{ErrorKind, LibError, Result};
pub use crate::models::{ConnectionMetadata, WsContext};

#[cfg(feature = "sqlx")]
pub use crate::models::{ConnectionLease, WsConnection, WsUserSession};
