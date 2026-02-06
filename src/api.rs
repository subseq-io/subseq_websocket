use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex as StdMutex};

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRef, State};
use axum::http::header::{COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::PgPool;
use subseq_auth::prelude::{MaybeAuthenticatedUser, ValidatesIdentity};
use subseq_auth::user_id::UserId;
use tokio::sync::{Mutex as TokioMutex, RwLock, mpsc};
use uuid::Uuid;

use crate::db;
use crate::error::{ErrorKind, LibError, Result};
use crate::models::{ConnectionMetadata, WsContext};

const WS_SESSION_COOKIE: &str = "subseq_ws_session";
const DEFAULT_ANON_SESSION_TTL_SECONDS: i64 = 300;

/// Inbound message stream item for one active websocket session.
#[derive(Debug, Clone)]
pub enum SessionIngressMessage {
    Connected(WsContext),
    Text {
        context: WsContext,
        payload: String,
    },
    Binary {
        context: WsContext,
        payload: Bytes,
    },
    UnauthorizedInbound {
        context: WsContext,
        message_kind: &'static str,
    },
    Disconnected(WsContext),
}

/// Manager for a single active websocket session.
///
/// Use `take_ingress_stream()` once to obtain a receiver you can consume with
/// `while let Some(message) = stream.recv().await { ... }`.
///
/// Keep an `Arc<SessionManager>` where you need to send egress payloads back to
/// that same session.
pub struct SessionManager {
    session_id: Uuid,
    hub: WsHub,
    ingress_tx: StdMutex<Option<mpsc::UnboundedSender<SessionIngressMessage>>>,
    ingress_rx: TokioMutex<Option<mpsc::UnboundedReceiver<SessionIngressMessage>>>,
}

impl SessionManager {
    fn new(session_id: Uuid, hub: WsHub) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            session_id,
            hub,
            ingress_tx: StdMutex::new(Some(tx)),
            ingress_rx: TokioMutex::new(Some(rx)),
        }
    }

    /// Return this manager's session identifier.
    pub fn session_id(&self) -> Uuid {
        self.session_id
    }

    /// Take the ingress stream receiver for this session.
    ///
    /// This can be taken only once. Subsequent calls return `None`.
    pub async fn take_ingress_stream(
        &self,
    ) -> Option<mpsc::UnboundedReceiver<SessionIngressMessage>> {
        self.ingress_rx.lock().await.take()
    }

    /// Send a JSON payload to all active connections in this session.
    pub async fn send_json<T: Serialize>(&self, payload: &T) -> Result<usize> {
        self.hub
            .send_json_to_session(self.session_id, payload)
            .await
    }

    /// Send a text payload to all active connections in this session.
    pub async fn send_text(&self, payload: impl Into<String>) -> usize {
        self.hub
            .send_text_to_session(self.session_id, payload)
            .await
    }

    /// Send a binary payload to all active connections in this session.
    pub async fn send_binary(&self, payload: Bytes) -> usize {
        self.hub
            .send_binary_to_session(self.session_id, payload)
            .await
    }

    fn push_ingress(&self, message: SessionIngressMessage) {
        if let Some(tx) = self.ingress_tx.lock().unwrap().as_ref() {
            let _ = tx.send(message);
        }
    }

    fn close_ingress(&self) {
        self.ingress_tx.lock().unwrap().take();
    }
}

/// In-memory fan-out hub keyed by active sessions and optionally by user.
///
/// - Session fan-out supports unauthenticated receive-only clients.
/// - User fan-out supports merged multi-tab behavior for authenticated users.
#[derive(Clone)]
pub struct WsHub {
    inner: Arc<RwLock<HubState>>,
}

#[derive(Default)]
struct HubState {
    users: HashMap<Uuid, HashMap<Uuid, mpsc::UnboundedSender<OutboundMessage>>>,
    sessions: HashMap<Uuid, HashMap<Uuid, mpsc::UnboundedSender<OutboundMessage>>>,
    managers: HashMap<Uuid, Arc<SessionManager>>,
}

impl Default for WsHub {
    fn default() -> Self {
        Self::new()
    }
}

impl WsHub {
    /// Create an empty websocket hub.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HubState::default())),
        }
    }

    /// Register a new connection under its session and optional user identity.
    pub async fn register(
        &self,
        user_id: Option<UserId>,
        session_id: Uuid,
        connection_id: Uuid,
        tx: mpsc::UnboundedSender<OutboundMessage>,
    ) -> Arc<SessionManager> {
        let hub = self.clone();
        let mut guard = self.inner.write().await;
        let manager = guard
            .managers
            .entry(session_id)
            .or_insert_with(|| Arc::new(SessionManager::new(session_id, hub)))
            .clone();

        guard
            .sessions
            .entry(session_id)
            .or_default()
            .insert(connection_id, tx.clone());

        if let Some(user_id) = user_id {
            guard
                .users
                .entry(user_id.0)
                .or_default()
                .insert(connection_id, tx);
        }

        manager
    }

    /// Unregister a connection from all fan-out indexes.
    pub async fn unregister(&self, user_id: Option<UserId>, session_id: Uuid, connection_id: Uuid) {
        let mut guard = self.inner.write().await;

        if let Some(session_map) = guard.sessions.get_mut(&session_id) {
            session_map.remove(&connection_id);
            if session_map.is_empty() {
                guard.sessions.remove(&session_id);
                if let Some(manager) = guard.managers.remove(&session_id) {
                    manager.close_ingress();
                }
            }
        }

        if let Some(user_id) = user_id {
            if let Some(user_map) = guard.users.get_mut(&user_id.0) {
                user_map.remove(&connection_id);
                if user_map.is_empty() {
                    guard.users.remove(&user_id.0);
                }
            }
        }
    }

    /// Move session-indexed connections into a user fan-out group.
    ///
    /// Call this after persisting auth state if a previously anonymous session
    /// should now be addressable by `user_id`.
    pub async fn associate_user_with_session(&self, session_id: Uuid, user_id: Uuid) -> usize {
        let mut guard = self.inner.write().await;

        let Some(session_map) = guard.sessions.get(&session_id) else {
            return 0;
        };
        let session_map = session_map.clone();

        let user_map = guard.users.entry(user_id).or_default();
        let mut linked = 0;

        for (connection_id, tx) in session_map {
            if user_map.insert(connection_id, tx).is_none() {
                linked += 1;
            }
        }

        linked
    }

    /// Get the active session manager for a session id.
    pub async fn session_manager(&self, session_id: Uuid) -> Option<Arc<SessionManager>> {
        let guard = self.inner.read().await;
        guard.managers.get(&session_id).cloned()
    }

    /// Serialize and send a JSON payload to all active connections for one user.
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

    /// Send a raw text payload to all active connections for one user.
    pub async fn send_text_to_user(&self, user_id: Uuid, payload: impl Into<String>) -> usize {
        self.send_to_user(user_id, OutboundMessage::Text(payload.into()))
            .await
    }

    /// Send a raw binary payload to all active connections for one user.
    pub async fn send_binary_to_user(&self, user_id: Uuid, payload: Bytes) -> usize {
        self.send_to_user(user_id, OutboundMessage::Binary(payload))
            .await
    }

    /// Serialize and send a JSON payload to all active connections for one session.
    pub async fn send_json_to_session<T: Serialize>(
        &self,
        session_id: Uuid,
        payload: &T,
    ) -> Result<usize> {
        let text = serde_json::to_string(payload)
            .map_err(|e| LibError::invalid("failed to serialize websocket json payload", e))?;
        Ok(self
            .send_to_session(session_id, OutboundMessage::Text(text))
            .await)
    }

    /// Send a raw text payload to all active connections for one session.
    pub async fn send_text_to_session(
        &self,
        session_id: Uuid,
        payload: impl Into<String>,
    ) -> usize {
        self.send_to_session(session_id, OutboundMessage::Text(payload.into()))
            .await
    }

    /// Send a raw binary payload to all active connections for one session.
    pub async fn send_binary_to_session(&self, session_id: Uuid, payload: Bytes) -> usize {
        self.send_to_session(session_id, OutboundMessage::Binary(payload))
            .await
    }

    /// Serialize and broadcast a JSON payload to all active connections.
    pub async fn broadcast_json<T: Serialize>(&self, payload: &T) -> Result<usize> {
        let text = serde_json::to_string(payload)
            .map_err(|e| LibError::invalid("failed to serialize websocket json payload", e))?;
        Ok(self.broadcast(OutboundMessage::Text(text)).await)
    }

    async fn send_to_user(&self, user_id: Uuid, payload: OutboundMessage) -> usize {
        let mut guard = self.inner.write().await;
        let mut delivered = 0;

        if let Some(user_map) = guard.users.get_mut(&user_id) {
            user_map.retain(|_, tx| {
                if tx.send(payload.clone()).is_ok() {
                    delivered += 1;
                    true
                } else {
                    false
                }
            });

            if user_map.is_empty() {
                guard.users.remove(&user_id);
            }
        }

        delivered
    }

    async fn send_to_session(&self, session_id: Uuid, payload: OutboundMessage) -> usize {
        let mut guard = self.inner.write().await;
        let mut delivered = 0;

        if let Some(session_map) = guard.sessions.get_mut(&session_id) {
            session_map.retain(|_, tx| {
                if tx.send(payload.clone()).is_ok() {
                    delivered += 1;
                    true
                } else {
                    false
                }
            });

            if session_map.is_empty() {
                guard.sessions.remove(&session_id);
            }
        }

        delivered
    }

    async fn broadcast(&self, payload: OutboundMessage) -> usize {
        let mut guard = self.inner.write().await;
        let mut delivered = 0;

        guard.sessions.retain(|_, session_map| {
            session_map.retain(|_, tx| {
                if tx.send(payload.clone()).is_ok() {
                    delivered += 1;
                    true
                } else {
                    false
                }
            });

            !session_map.is_empty()
        });

        guard.users.retain(|_, user_map| !user_map.is_empty());
        let active_sessions: HashSet<Uuid> = guard.sessions.keys().copied().collect();
        guard
            .managers
            .retain(|session_id, _| active_sessions.contains(session_id));

        delivered
    }
}

/// Outbound payload envelope for active websocket connections.
#[derive(Clone)]
pub enum OutboundMessage {
    Text(String),
    Binary(Bytes),
    Close,
}

/// Application state adapter for accessing a shared SQLx pool.
pub trait HasPool {
    fn pool(&self) -> Arc<PgPool>;
}

/// Application state adapter for accessing the websocket hub.
pub trait HasWsHub {
    fn ws_hub(&self) -> WsHub;
}

/// Typed JSON dispatcher entrypoint.
///
/// Implement this on your inbound JSON enum and route each variant to internal
/// handlers so callers keep a typed boundary as soon as deserialization
/// succeeds.
pub trait JsonDispatch<S>: Send + 'static {
    fn dispatch(self, app: &S, context: WsContext) -> impl Future<Output = Result<()>> + Send;
}

/// Hook points for websocket lifecycle and payload handling.
///
/// `IncomingJson` should normally be an internally tagged enum, for example:
/// `#[serde(tag = "type", rename_all = "snake_case")]`.
pub trait HandlesWebSocketEvents: Sized + Send + Sync {
    type IncomingJson: DeserializeOwned + JsonDispatch<Self> + Send + 'static;

    /// Called once after a websocket has been registered.
    fn on_connect(&self, _context: WsContext) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }

    /// Called for every incoming binary frame from authenticated clients.
    fn on_binary(
        &self,
        _context: WsContext,
        _payload: Bytes,
    ) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }

    /// Called once after websocket shutdown and unregistration.
    fn on_disconnect(&self, _context: WsContext) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }
}

/// Required capabilities for mounting websocket routes.
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
    auth_user: MaybeAuthenticatedUser,
    headers: HeaderMap,
) -> Response
where
    S: WsApp,
{
    let user_id = auth_user.0.map(|u| u.id());
    let metadata = metadata_from_headers(&headers);
    let anonymous_session_ttl_seconds = anonymous_session_ttl_seconds();
    let requested_session_id = session_id_from_cookie(&headers);

    if let (Some(user_id), Some(session_id)) = (user_id, requested_session_id) {
        match db::associate_user_with_session(app.pool().as_ref(), session_id, user_id).await {
            Ok(_) => {
                let linked = app
                    .ws_hub()
                    .associate_user_with_session(session_id, user_id.0)
                    .await;
                tracing::debug!(
                    user_id = %user_id,
                    session_id = %session_id,
                    linked,
                    "associated websocket session with authenticated user"
                );
            }
            Err(err) if matches!(err.kind, ErrorKind::NotFound) => {}
            Err(err) => {
                tracing::warn!(
                    user_id = %user_id,
                    session_id = %session_id,
                    error = %err,
                    "failed to associate websocket session with authenticated user"
                );
            }
        }
    }

    let session = match db::open_or_resume_session(
        app.pool().as_ref(),
        user_id,
        if user_id.is_none() {
            requested_session_id
        } else {
            None
        },
        &metadata,
    )
    .await
    {
        Ok(session) => session,
        Err(err) => {
            tracing::error!(
                user_id = %user_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "anonymous".to_string()),
                error = %err,
                "failed to open websocket session"
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "websocket session setup failed",
            )
                .into_response();
        }
    };

    let mut response = ws
        .on_upgrade(move |socket| async move {
            run_socket(
                app,
                socket,
                user_id,
                session.session_id,
                metadata,
                anonymous_session_ttl_seconds,
            )
            .await;
        })
        .into_response();

    if let Ok(set_cookie) = HeaderValue::from_str(&session_cookie_header_value(
        session.session_id,
        anonymous_session_ttl_seconds,
    )) {
        response.headers_mut().append(SET_COOKIE, set_cookie);
    }

    response
}

async fn run_socket<S>(
    app: S,
    socket: WebSocket,
    user_id: Option<UserId>,
    session_id: Uuid,
    metadata: ConnectionMetadata,
    anonymous_session_ttl_seconds: i64,
) where
    S: WsApp,
{
    let pool = app.pool();
    let user_tag = user_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "anonymous".to_string());

    let connection =
        match db::register_connection(pool.as_ref(), session_id, user_id, &metadata).await {
            Ok(connection) => connection,
            Err(err) => {
                tracing::error!(
                    user_id = %user_tag,
                    session_id = %session_id,
                    error = %err,
                    "failed to register websocket connection"
                );
                return;
            }
        };

    let context = WsContext::new(user_id, session_id, connection.connection_id, metadata);

    let (tx, rx) = mpsc::unbounded_channel();
    let hub = app.ws_hub();
    let session_manager = hub
        .register(user_id, session_id, connection.connection_id, tx.clone())
        .await;
    session_manager.push_ingress(SessionIngressMessage::Connected(context.clone()));

    if let Err(err) = app.on_connect(context.clone()).await {
        tracing::warn!(
            user_id = %user_tag,
            session_id = %session_id,
            connection_id = %connection.connection_id,
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
                    user_id = %user_tag,
                    session_id = %session_id,
                    connection_id = %connection.connection_id,
                    error = %err,
                    "websocket receive error"
                );
                break;
            }
        };

        if let Err(err) = db::touch_connection(pool.as_ref(), connection.connection_id).await {
            tracing::debug!(
                connection_id = %connection.connection_id,
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

                if user_id.is_none() {
                    session_manager.push_ingress(SessionIngressMessage::UnauthorizedInbound {
                        context: context.clone(),
                        message_kind: "text",
                    });
                    if !reject_unauthorized_incoming(
                        &tx,
                        session_id,
                        connection.connection_id,
                        "text",
                    ) {
                        break;
                    }
                    continue;
                }

                session_manager.push_ingress(SessionIngressMessage::Text {
                    context: context.clone(),
                    payload: text.clone(),
                });

                match serde_json::from_str::<S::IncomingJson>(&text) {
                    Ok(json_message) => {
                        if let Err(err) = json_message.dispatch(&app, context.clone()).await {
                            tracing::warn!(
                                user_id = %user_tag,
                                session_id = %session_id,
                                connection_id = %connection.connection_id,
                                error = %err,
                                "websocket json dispatch failed"
                            );
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            user_id = %user_tag,
                            session_id = %session_id,
                            connection_id = %connection.connection_id,
                            error = %err,
                            "failed to deserialize websocket json message"
                        );
                    }
                }
            }
            Message::Binary(payload) => {
                if user_id.is_none() {
                    session_manager.push_ingress(SessionIngressMessage::UnauthorizedInbound {
                        context: context.clone(),
                        message_kind: "binary",
                    });
                    if !reject_unauthorized_incoming(
                        &tx,
                        session_id,
                        connection.connection_id,
                        "binary",
                    ) {
                        break;
                    }
                    continue;
                }

                session_manager.push_ingress(SessionIngressMessage::Binary {
                    context: context.clone(),
                    payload: payload.clone(),
                });

                if let Err(err) = app.on_binary(context.clone(), payload).await {
                    tracing::warn!(
                        user_id = %user_tag,
                        session_id = %session_id,
                        connection_id = %connection.connection_id,
                        error = %err,
                        "websocket binary handler failed"
                    );
                }
            }
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }

    session_manager.push_ingress(SessionIngressMessage::Disconnected(context.clone()));
    hub.unregister(user_id, session_id, connection.connection_id)
        .await;
    let _ = tx.send(OutboundMessage::Close);
    drop(tx);
    let _ = writer.await;

    if let Err(err) = app.on_disconnect(context.clone()).await {
        tracing::warn!(
            user_id = %user_tag,
            session_id = %session_id,
            connection_id = %connection.connection_id,
            error = %err,
            "websocket on_disconnect failed"
        );
    }

    if let Err(err) = db::close_connection(
        pool.as_ref(),
        connection.connection_id,
        anonymous_session_ttl_seconds,
    )
    .await
    {
        tracing::warn!(
            user_id = %user_tag,
            session_id = %session_id,
            connection_id = %connection.connection_id,
            error = %err,
            "failed to close websocket connection"
        );
    }
}

fn reject_unauthorized_incoming(
    tx: &mpsc::UnboundedSender<OutboundMessage>,
    session_id: Uuid,
    connection_id: Uuid,
    message_kind: &'static str,
) -> bool {
    tracing::debug!(
        session_id = %session_id,
        connection_id = %connection_id,
        message_kind,
        "rejecting websocket payload from unauthenticated connection"
    );
    tx.send(OutboundMessage::Text("UNAUTHORIZED".to_string()))
        .is_ok()
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

fn session_id_from_cookie(headers: &HeaderMap) -> Option<Uuid> {
    for cookie_header in headers.get_all(COOKIE) {
        let raw_cookie = cookie_header.to_str().ok()?;
        for pair in raw_cookie.split(';') {
            let pair = pair.trim();
            let (name, value) = pair.split_once('=')?;
            if name == WS_SESSION_COOKIE {
                if let Ok(session_id) = Uuid::parse_str(value) {
                    return Some(session_id);
                }
            }
        }
    }
    None
}

fn session_cookie_header_value(session_id: Uuid, anonymous_session_ttl_seconds: i64) -> String {
    let ttl = anonymous_session_ttl_seconds.max(1);
    format!(
        "{name}={value}; Path=/; Max-Age={ttl}; HttpOnly; SameSite=Lax",
        name = WS_SESSION_COOKIE,
        value = session_id
    )
}

fn anonymous_session_ttl_seconds() -> i64 {
    std::env::var("WS_ANON_SESSION_TTL_SECONDS")
        .ok()
        .and_then(|raw| raw.parse::<i64>().ok())
        .filter(|ttl| *ttl > 0)
        .unwrap_or(DEFAULT_ANON_SESSION_TTL_SECONDS)
}

impl<S> FromRef<S> for WsHub
where
    S: HasWsHub,
{
    fn from_ref(input: &S) -> Self {
        input.ws_hub()
    }
}

/// Build websocket routes for this library.
///
/// Exposes:
/// - `GET /ws`
///
/// `AuthenticatedUser` is optional. Unauthenticated sockets are accepted but
/// treated as receive-only; incoming text/binary payloads (except keepalive
/// `PING`) are rejected.
///
/// The server sets `subseq_ws_session` as an HTTP-only cookie. Anonymous
/// sessions use this cookie for reconnect continuity and can be reclaimed by an
/// authenticated user if `associate_user_with_session` is called before expiry.
///
/// Anonymous TTL can be configured with `WS_ANON_SESSION_TTL_SECONDS`.
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
