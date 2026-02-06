use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRef, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::get;
use bytes::Bytes;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::PgPool;
use subseq_auth::prelude::{AuthenticatedUser, ValidatesIdentity};
use tokio::sync::{RwLock, mpsc};
use uuid::Uuid;

use crate::db;
use crate::error::{LibError, Result};
use crate::models::{ConnectionMetadata, WsContext};

#[derive(Clone)]
pub struct WsHub {
    inner: Arc<RwLock<HashMap<Uuid, HashMap<Uuid, mpsc::UnboundedSender<OutboundMessage>>>>>,
}

impl Default for WsHub {
    fn default() -> Self {
        Self::new()
    }
}

impl WsHub {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn register(
        &self,
        user_id: Uuid,
        connection_id: Uuid,
        tx: mpsc::UnboundedSender<OutboundMessage>,
    ) {
        let mut guard = self.inner.write().await;
        let user_map = guard.entry(user_id).or_default();
        user_map.insert(connection_id, tx);
    }

    pub async fn unregister(&self, user_id: Uuid, connection_id: Uuid) {
        let mut guard = self.inner.write().await;
        if let Some(user_map) = guard.get_mut(&user_id) {
            user_map.remove(&connection_id);
            if user_map.is_empty() {
                guard.remove(&user_id);
            }
        }
    }

    pub async fn send_json_to_user<T: Serialize>(
        &self,
        user_id: Uuid,
        payload: &T,
    ) -> Result<usize> {
        let text = serde_json::to_string(payload)
            .map_err(|e| LibError::invalid("failed to serialize websocket json payload", e))?;
        Ok(self
            .send_to_user(user_id, OutboundMessage::Text(text))
            .await)
    }

    pub async fn send_text_to_user(&self, user_id: Uuid, payload: impl Into<String>) -> usize {
        self.send_to_user(user_id, OutboundMessage::Text(payload.into()))
            .await
    }

    pub async fn send_binary_to_user(&self, user_id: Uuid, payload: Bytes) -> usize {
        self.send_to_user(user_id, OutboundMessage::Binary(payload))
            .await
    }

    pub async fn broadcast_json<T: Serialize>(&self, payload: &T) -> Result<usize> {
        let text = serde_json::to_string(payload)
            .map_err(|e| LibError::invalid("failed to serialize websocket json payload", e))?;
        Ok(self.broadcast(OutboundMessage::Text(text)).await)
    }

    async fn send_to_user(&self, user_id: Uuid, payload: OutboundMessage) -> usize {
        let mut guard = self.inner.write().await;
        let mut delivered = 0;

        if let Some(user_map) = guard.get_mut(&user_id) {
            user_map.retain(|_, tx| {
                if tx.send(payload.clone()).is_ok() {
                    delivered += 1;
                    true
                } else {
                    false
                }
            });

            if user_map.is_empty() {
                guard.remove(&user_id);
            }
        }

        delivered
    }

    async fn broadcast(&self, payload: OutboundMessage) -> usize {
        let mut guard = self.inner.write().await;
        let mut delivered = 0;

        guard.retain(|_, user_map| {
            user_map.retain(|_, tx| {
                if tx.send(payload.clone()).is_ok() {
                    delivered += 1;
                    true
                } else {
                    false
                }
            });
            !user_map.is_empty()
        });

        delivered
    }
}

#[derive(Clone)]
pub enum OutboundMessage {
    Text(String),
    Binary(Bytes),
    Close,
}

pub trait HasPool {
    fn pool(&self) -> Arc<PgPool>;
}

pub trait HasWsHub {
    fn ws_hub(&self) -> WsHub;
}

pub trait JsonDispatch<S>: Send + 'static {
    fn dispatch(self, app: &S, context: WsContext) -> impl Future<Output = Result<()>> + Send;
}

pub trait HandlesWebSocketEvents: Sized + Send + Sync {
    type IncomingJson: DeserializeOwned + JsonDispatch<Self> + Send + 'static;

    fn on_connect(&self, _context: WsContext) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }

    fn on_binary(
        &self,
        _context: WsContext,
        _payload: Bytes,
    ) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }

    fn on_disconnect(&self, _context: WsContext) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }
}

pub trait WsApp:
    HasPool + HasWsHub + ValidatesIdentity + HandlesWebSocketEvents + Clone + Send + Sync + 'static
{
}

impl<T> WsApp for T where
    T: HasPool
        + HasWsHub
        + ValidatesIdentity
        + HandlesWebSocketEvents
        + Clone
        + Send
        + Sync
        + 'static
{
}

async fn ws_handler<S>(
    ws: WebSocketUpgrade,
    State(app): State<S>,
    auth_user: AuthenticatedUser,
    headers: HeaderMap,
) -> impl IntoResponse
where
    S: WsApp,
{
    let user_id = auth_user.id().0;
    let metadata = metadata_from_headers(&headers);

    ws.on_upgrade(move |socket| async move {
        run_socket(app, socket, user_id, metadata).await;
    })
}

async fn run_socket<S>(app: S, socket: WebSocket, user_id: Uuid, metadata: ConnectionMetadata)
where
    S: WsApp,
{
    let pool = app.pool();
    let lease = match db::open_connection(pool.as_ref(), user_id, &metadata).await {
        Ok(lease) => lease,
        Err(err) => {
            tracing::error!(user_id = %user_id, error = %err, "failed to open websocket connection");
            return;
        }
    };

    let context = WsContext::new(
        user_id,
        lease.session.session_id,
        lease.connection.connection_id,
        metadata,
    );

    let (tx, rx) = mpsc::unbounded_channel();
    let hub = app.ws_hub();
    hub.register(user_id, lease.connection.connection_id, tx.clone())
        .await;

    if let Err(err) = app.on_connect(context.clone()).await {
        tracing::warn!(
            user_id = %user_id,
            connection_id = %lease.connection.connection_id,
            error = %err,
            "websocket on_connect failed"
        );
    }

    let (socket_tx, mut socket_rx) = socket.split();
    let writer = tokio::spawn(write_outbound_loop(socket_tx, rx));

    while let Some(message_result) = socket_rx.next().await {
        let message = match message_result {
            Ok(message) => message,
            Err(err) => {
                tracing::warn!(
                    user_id = %user_id,
                    connection_id = %lease.connection.connection_id,
                    error = %err,
                    "websocket receive error"
                );
                break;
            }
        };

        if let Err(err) = db::touch_connection(pool.as_ref(), lease.connection.connection_id).await
        {
            tracing::debug!(
                connection_id = %lease.connection.connection_id,
                error = %err,
                "failed to update websocket heartbeat"
            );
        }

        match message {
            Message::Text(text) => {
                let text = text.to_string();
                if let Some(reply) = keepalive_reply_for_text(&text) {
                    if tx.send(OutboundMessage::Text(reply)).is_err() {
                        break;
                    }
                    continue;
                }

                match serde_json::from_str::<S::IncomingJson>(&text) {
                    Ok(json_message) => {
                        if let Err(err) = json_message.dispatch(&app, context.clone()).await {
                            tracing::warn!(
                                user_id = %user_id,
                                connection_id = %lease.connection.connection_id,
                                error = %err,
                                "websocket json dispatch failed"
                            );
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            user_id = %user_id,
                            connection_id = %lease.connection.connection_id,
                            error = %err,
                            "failed to deserialize websocket json message"
                        );
                    }
                }
            }
            Message::Binary(payload) => {
                if let Err(err) = app.on_binary(context.clone(), payload).await {
                    tracing::warn!(
                        user_id = %user_id,
                        connection_id = %lease.connection.connection_id,
                        error = %err,
                        "websocket binary handler failed"
                    );
                }
            }
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }

    hub.unregister(user_id, lease.connection.connection_id)
        .await;
    let _ = tx.send(OutboundMessage::Close);
    drop(tx);
    let _ = writer.await;

    if let Err(err) = app.on_disconnect(context.clone()).await {
        tracing::warn!(
            user_id = %user_id,
            connection_id = %lease.connection.connection_id,
            error = %err,
            "websocket on_disconnect failed"
        );
    }

    if let Err(err) = db::close_connection(pool.as_ref(), lease.connection.connection_id).await {
        tracing::warn!(
            user_id = %user_id,
            connection_id = %lease.connection.connection_id,
            error = %err,
            "failed to close websocket connection"
        );
    }
}

async fn write_outbound_loop(
    mut socket_tx: SplitSink<WebSocket, Message>,
    mut rx: mpsc::UnboundedReceiver<OutboundMessage>,
) {
    while let Some(message) = rx.recv().await {
        let message = match message {
            OutboundMessage::Text(payload) => Message::Text(payload.into()),
            OutboundMessage::Binary(payload) => Message::Binary(payload),
            OutboundMessage::Close => Message::Close(None),
        };

        if socket_tx.send(message).await.is_err() {
            break;
        }
    }
}

fn metadata_from_headers(headers: &HeaderMap) -> ConnectionMetadata {
    ConnectionMetadata {
        user_agent: headers
            .get("user-agent")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned),
        x_forwarded_for: headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned),
        x_real_ip: headers
            .get("x-real-ip")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned),
    }
}

impl<S> FromRef<S> for WsHub
where
    S: HasWsHub,
{
    fn from_ref(input: &S) -> Self {
        input.ws_hub()
    }
}

pub fn routes<S>() -> Router<S>
where
    S: WsApp,
{
    Router::new().route("/ws", get(ws_handler::<S>))
}

fn keepalive_reply_for_text(input: &str) -> Option<String> {
    let trimmed = input.trim();

    if trimmed.eq_ignore_ascii_case("PING") {
        return Some("PONG".to_string());
    }

    let (verb, rest) = trimmed.split_once(char::is_whitespace)?;
    if verb.eq_ignore_ascii_case("PING") {
        if rest.trim().is_empty() {
            Some("PONG".to_string())
        } else {
            Some(format!("PONG {}", rest.trim()))
        }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::keepalive_reply_for_text;

    #[test]
    fn keepalive_basic_ping() {
        assert_eq!(keepalive_reply_for_text("PING"), Some("PONG".to_string()));
    }

    #[test]
    fn keepalive_is_case_insensitive_and_trimmed() {
        assert_eq!(
            keepalive_reply_for_text("  ping  "),
            Some("PONG".to_string())
        );
    }

    #[test]
    fn keepalive_forwards_suffix_payload() {
        assert_eq!(
            keepalive_reply_for_text("PING 1678901234"),
            Some("PONG 1678901234".to_string())
        );
    }

    #[test]
    fn non_keepalive_text_is_not_handled() {
        assert_eq!(keepalive_reply_for_text("{\"type\":\"PING\"}"), None);
        assert_eq!(keepalive_reply_for_text("PONG"), None);
    }
}
