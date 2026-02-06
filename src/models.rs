use serde::{Deserialize, Serialize};
use subseq_auth::user_id::UserId;
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
    pub user_id: Option<UserId>,
    pub session_id: Uuid,
    pub connection_id: Uuid,
    pub metadata: ConnectionMetadata,
}

impl WsContext {
    /// Build a new handler context.
    pub fn new(
        user_id: Option<UserId>,
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
/// Persistent logical websocket session.
///
/// Anonymous sessions have `user_id = None`.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct WsUserSession {
    pub session_id: Uuid,
    pub user_id: Option<Uuid>,
    pub connected_at: chrono::DateTime<chrono::Utc>,
    pub last_seen_at: chrono::DateTime<chrono::Utc>,
    pub disconnected_at: Option<chrono::DateTime<chrono::Utc>>,
    pub reconnect_count: i64,
    pub metadata: serde_json::Value,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[cfg(feature = "sqlx")]
/// Persistent physical websocket connection row.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct WsConnection {
    pub connection_id: Uuid,
    pub user_id: Option<Uuid>,
    pub session_id: Uuid,
    pub connected_at: chrono::DateTime<chrono::Utc>,
    pub last_seen_at: chrono::DateTime<chrono::Utc>,
    pub disconnected_at: Option<chrono::DateTime<chrono::Utc>>,
    pub metadata: serde_json::Value,
}

#[cfg(feature = "sqlx")]
/// Combined session + connection lease.
#[derive(Debug, Clone)]
pub struct ConnectionLease {
    pub session: WsUserSession,
    pub connection: WsConnection,
}
