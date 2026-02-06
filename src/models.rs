use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Best-effort metadata captured from websocket upgrade headers.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionMetadata {
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub x_forwarded_for: Option<String>,
    #[serde(default)]
    pub x_real_ip: Option<String>,
}

impl ConnectionMetadata {
    /// Convert metadata into JSON for persistence.
    pub fn as_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}))
    }
}

/// Runtime context passed to websocket handlers for each connection.
#[derive(Debug, Clone)]
pub struct WsContext {
    pub user_id: Uuid,
    pub session_id: Uuid,
    pub connection_id: Uuid,
    pub metadata: ConnectionMetadata,
}

impl WsContext {
    /// Build a new handler context.
    pub fn new(
        user_id: Uuid,
        session_id: Uuid,
        connection_id: Uuid,
        metadata: ConnectionMetadata,
    ) -> Self {
        Self {
            user_id,
            session_id,
            connection_id,
            metadata,
        }
    }
}

#[cfg(feature = "sqlx")]
/// Persistent logical websocket session keyed by `user_id`.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct WsUserSession {
    pub user_id: Uuid,
    pub session_id: Uuid,
    pub connected_at: chrono::DateTime<chrono::Utc>,
    pub last_seen_at: chrono::DateTime<chrono::Utc>,
    pub disconnected_at: Option<chrono::DateTime<chrono::Utc>>,
    pub reconnect_count: i64,
    pub metadata: serde_json::Value,
}

#[cfg(feature = "sqlx")]
/// Persistent physical websocket connection row.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct WsConnection {
    pub connection_id: Uuid,
    pub user_id: Uuid,
    pub session_id: Uuid,
    pub connected_at: chrono::DateTime<chrono::Utc>,
    pub last_seen_at: chrono::DateTime<chrono::Utc>,
    pub disconnected_at: Option<chrono::DateTime<chrono::Utc>>,
    pub metadata: serde_json::Value,
}

#[cfg(feature = "sqlx")]
/// Combined result from opening a websocket connection.
#[derive(Debug, Clone)]
pub struct ConnectionLease {
    pub session: WsUserSession,
    pub connection: WsConnection,
}
