use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Query, Request, State};
use axum::middleware::{self, Next};
use axum::response::{Html, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use subseq_auth::prelude::{
    AuthenticatedUser, ClaimsVerificationError, CoreIdToken, CoreIdTokenClaims, OidcToken,
    ValidatesIdentity,
};
use subseq_websocket::prelude::{
    HandlesWebSocketEvents, HasPool, HasWsHub, JsonDispatch, WsContext, WsHub,
    create_websocket_tables, routes,
};
use uuid::Uuid;

/// `GET /` query options for the demo page.
#[derive(Debug, Deserialize)]
struct ChatPageQuery {
    demo_user: Option<Uuid>,
}

#[derive(Clone)]
struct AppState {
    pool: Arc<PgPool>,
    hub: WsHub,
}

impl HasPool for AppState {
    fn pool(&self) -> Arc<PgPool> {
        self.pool.clone()
    }
}

impl HasWsHub for AppState {
    fn ws_hub(&self) -> WsHub {
        self.hub.clone()
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ChatClientMessage {
    ChatSend { text: String },
    Typing { is_typing: bool },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ChatServerMessage {
    System { message: String },
    Chat { from: String, text: String },
    Typing { from: String, is_typing: bool },
}

impl JsonDispatch<AppState> for ChatClientMessage {
    async fn dispatch(
        self,
        app: &AppState,
        context: WsContext,
    ) -> subseq_websocket::error::Result<()> {
        match self {
            ChatClientMessage::ChatSend { text } => {
                let payload = ChatServerMessage::Chat {
                    from: short_user(context.user_id),
                    text,
                };
                app.ws_hub().broadcast_json(&payload).await?;
            }
            ChatClientMessage::Typing { is_typing } => {
                let payload = ChatServerMessage::Typing {
                    from: short_user(context.user_id),
                    is_typing,
                };
                app.ws_hub().broadcast_json(&payload).await?;
            }
        }

        Ok(())
    }
}

impl HandlesWebSocketEvents for AppState {
    type IncomingJson = ChatClientMessage;

    async fn on_connect(&self, context: WsContext) -> subseq_websocket::error::Result<()> {
        let payload = ChatServerMessage::System {
            message: format!("{} joined", short_user(context.user_id)),
        };
        self.ws_hub().broadcast_json(&payload).await?;
        Ok(())
    }

    async fn on_binary(
        &self,
        context: WsContext,
        payload: bytes::Bytes,
    ) -> subseq_websocket::error::Result<()> {
        let notice = ChatServerMessage::System {
            message: format!(
                "{} sent {} bytes of binary data",
                short_user(context.user_id),
                payload.len()
            ),
        };
        self.ws_hub()
            .send_json_to_user(context.user_id, &notice)
            .await?;
        Ok(())
    }

    async fn on_disconnect(&self, context: WsContext) -> subseq_websocket::error::Result<()> {
        let payload = ChatServerMessage::System {
            message: format!("{} left", short_user(context.user_id)),
        };
        self.ws_hub().broadcast_json(&payload).await?;
        Ok(())
    }
}

impl ValidatesIdentity for AppState {
    fn validate_bearer(
        &self,
        token: &str,
    ) -> Result<(CoreIdToken, CoreIdTokenClaims), ClaimsVerificationError> {
        let user_id = token
            .strip_prefix("demo:")
            .and_then(|raw| Uuid::parse_str(raw).ok())
            .unwrap_or(demo_user_id());
        demo_identity(user_id)
    }

    fn validate_token(
        &self,
        _token: &OidcToken,
    ) -> Result<(CoreIdToken, CoreIdTokenClaims), ClaimsVerificationError> {
        demo_identity(demo_user_id())
    }

    async fn refresh_token(&self, token: OidcToken) -> anyhow::Result<OidcToken> {
        Ok(token)
    }
}

#[derive(Debug, Deserialize)]
struct DemoAuthQuery {
    demo_user: Option<Uuid>,
}

async fn demo_auth_middleware(
    State(_state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    if let Some(user_id) = request
        .uri()
        .query()
        .and_then(demo_user_from_query)
        .or_else(|| {
            request
                .headers()
                .get("x-demo-user")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| Uuid::parse_str(v).ok())
        })
    {
        if let Ok((token, claims)) = demo_identity(user_id) {
            if let Ok(auth_user) = AuthenticatedUser::from_claims(token, claims).await {
                request.extensions_mut().insert(auth_user);
            }
        }
    }

    next.run(request).await
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({"ok": true}))
}

async fn chat_page(Query(query): Query<ChatPageQuery>) -> Html<String> {
    let initial_user = query.demo_user.unwrap_or_else(demo_user_id);
    Html(format!(
        r#"<!doctype html>
<html>
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Websocket Chat Demo</title>
  <style>
    body {{ font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace; max-width: 920px; margin: 0 auto; padding: 20px; background: #101417; color: #e4eef5; }}
    h1 {{ margin-top: 0; }}
    .row {{ display: flex; gap: 8px; flex-wrap: wrap; margin-bottom: 12px; }}
    input, button {{ background: #1b2329; border: 1px solid #2e3a44; color: #e4eef5; padding: 8px 10px; border-radius: 8px; }}
    input {{ flex: 1; min-width: 240px; }}
    button {{ cursor: pointer; }}
    #log {{ border: 1px solid #2e3a44; border-radius: 8px; background: #0a0f12; padding: 12px; min-height: 280px; overflow: auto; white-space: pre-wrap; }}
    .hint {{ color: #9fb2c3; font-size: 13px; }}
  </style>
</head>
<body>
  <h1>Stripped-down Chat</h1>
  <p class="hint">Open this page in two tabs or browsers and set different user IDs.</p>
  <div class="row">
    <input id="user" value="{}" />
    <button id="connect">Connect</button>
  </div>
  <div class="row">
    <input id="msg" placeholder="Say something" />
    <button id="send">Send</button>
  </div>
  <div id="log"></div>

<script>
const logEl = document.getElementById('log');
const userEl = document.getElementById('user');
const msgEl = document.getElementById('msg');
const connectEl = document.getElementById('connect');
const sendEl = document.getElementById('send');
let ws = null;
let keepalive = null;

function log(line) {{
  const now = new Date().toISOString().slice(11, 19);
  logEl.textContent += `[${{now}}] ${{line}}\n`;
  logEl.scrollTop = logEl.scrollHeight;
}}

function connect() {{
  const user = userEl.value.trim();
  if (!user) {{
    log('demo_user is required');
    return;
  }}

  if (ws && ws.readyState === WebSocket.OPEN) {{
    ws.close();
  }}

  const proto = window.location.protocol === 'https:' ? 'wss' : 'ws';
  const url = `${{proto}}://${{window.location.host}}/ws?demo_user=${{encodeURIComponent(user)}}`;
  ws = new WebSocket(url);

  ws.onopen = () => {{
    log(`connected as ${{user}}`);
    if (keepalive) clearInterval(keepalive);
    keepalive = setInterval(() => {{
      if (ws && ws.readyState === WebSocket.OPEN) {{
        ws.send(`PING ${{Date.now()}}`);
      }}
    }}, 25000);
  }};

  ws.onmessage = (event) => {{
    if (typeof event.data !== 'string') {{
      log(`binary message: ${{event.data.size || 0}} bytes`);
      return;
    }}

    if (event.data.startsWith('PONG')) {{
      return;
    }}

    try {{
      const parsed = JSON.parse(event.data);
      log(JSON.stringify(parsed));
    }} catch (_) {{
      log(event.data);
    }}
  }};

  ws.onclose = () => {{
    log('socket closed');
    if (keepalive) {{
      clearInterval(keepalive);
      keepalive = null;
    }}
  }};

  ws.onerror = () => log('socket error');
}}

function sendMessage() {{
  if (!ws || ws.readyState !== WebSocket.OPEN) {{
    log('connect first');
    return;
  }}

  const text = msgEl.value.trim();
  if (!text) return;

  ws.send(JSON.stringify({{ type: 'chat_send', text }}));
  msgEl.value = '';
}}

connectEl.addEventListener('click', connect);
sendEl.addEventListener('click', sendMessage);
msgEl.addEventListener('keydown', (event) => {{
  if (event.key === 'Enter') sendMessage();
}});

connect();
</script>
</body>
</html>
"#,
        initial_user
    ))
}

fn demo_user_id() -> Uuid {
    Uuid::from_u128(1)
}

fn demo_user_from_query(query: &str) -> Option<Uuid> {
    serde_urlencoded::from_str::<DemoAuthQuery>(query)
        .ok()
        .and_then(|parsed| parsed.demo_user)
}

fn demo_identity(
    user_id: Uuid,
) -> Result<(CoreIdToken, CoreIdTokenClaims), ClaimsVerificationError> {
    let token = CoreIdToken::from_str(SAMPLE_ID_TOKEN)
        .map_err(|err| ClaimsVerificationError::Other(err.to_string()))?;

    let now = Utc::now().timestamp();
    let claims_value = json!({
        "iss": "https://example.local",
        "aud": ["subseq-websocket-chat-example"],
        "exp": now + 3600,
        "iat": now,
        "sub": user_id.to_string(),
        "preferred_username": format!("demo-{}", &user_id.to_string()[..8]),
        "email": format!("{}@example.local", &user_id.to_string()[..8]),
        "email_verified": true
    });

    let claims = serde_json::from_value::<CoreIdTokenClaims>(claims_value)
        .map_err(|err| ClaimsVerificationError::Other(err.to_string()))?;

    Ok((token, claims))
}

fn short_user(user_id: Uuid) -> String {
    user_id.to_string()[..8].to_string()
}

const SAMPLE_ID_TOKEN: &str = concat!(
    "eyJhbGciOiJSUzI1NiJ9.",
    "eyJpc3MiOiJodHRwczovL3NlcnZlci5leGFtcGxlLmNvbSIsImF1ZCI6WyJzNkJoZ",
    "FJrcXQzIl0sImV4cCI6MTMxMTI4MTk3MCwiaWF0IjoxMzExMjgwOTcwLCJzdWIiOi",
    "IyNDQwMDMyMCIsInRmYV9tZXRob2QiOiJ1MmYifQ.",
    "aW52YWxpZF9zaWduYXR1cmU"
);

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let database_url = std::env::var("DATABASE_URL")
        .context("set DATABASE_URL to a postgres connection string")?;
    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:3000".to_string());
    let addr: SocketAddr = bind_addr.parse().context("invalid BIND_ADDR")?;

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await
        .context("failed to connect postgres")?;

    create_websocket_tables(&pool)
        .await
        .context("failed to run websocket migrations")?;

    let state = AppState {
        pool: Arc::new(pool),
        hub: WsHub::new(),
    };

    let app = Router::new()
        .route("/", get(chat_page))
        .route("/healthz", get(health))
        .merge(routes::<AppState>())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            demo_auth_middleware,
        ))
        .with_state(state);

    tracing::info!("chat demo listening on {}", addr);
    axum_server::bind(addr)
        .serve(app.into_make_service())
        .await
        .context("server failed")?;

    Ok(())
}
