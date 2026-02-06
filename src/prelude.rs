//! Convenience re-exports for implementing websocket services.

#[cfg(feature = "api")]
pub use crate::api::{
    HandlesWebSocketEvents, HasPool, HasWsHub, JsonDispatch, OutboundMessage, WsApp, WsHub, routes,
};

#[cfg(feature = "sqlx")]
pub use crate::db::{
    active_connection_count, close_connection, create_websocket_tables, get_user_session,
    open_connection, touch_connection,
};

pub use crate::error::{ErrorKind, LibError, Result};
pub use crate::models::{ConnectionMetadata, WsContext};

#[cfg(feature = "sqlx")]
pub use crate::models::{ConnectionLease, WsConnection, WsUserSession};
