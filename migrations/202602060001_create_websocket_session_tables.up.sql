CREATE SCHEMA IF NOT EXISTS websocket;

CREATE TABLE IF NOT EXISTS websocket.user_sessions (
    user_id UUID PRIMARY KEY,
    session_id UUID NOT NULL UNIQUE,
    connected_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    disconnected_at TIMESTAMPTZ,
    reconnect_count BIGINT NOT NULL DEFAULT 0,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE TABLE IF NOT EXISTS websocket.connections (
    connection_id UUID PRIMARY KEY,
    user_id UUID NOT NULL,
    session_id UUID NOT NULL,
    connected_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    disconnected_at TIMESTAMPTZ,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE INDEX IF NOT EXISTS idx_ws_connections_user_id
    ON websocket.connections (user_id);

CREATE INDEX IF NOT EXISTS idx_ws_connections_session_id
    ON websocket.connections (session_id);

CREATE INDEX IF NOT EXISTS idx_ws_connections_disconnected_at
    ON websocket.connections (disconnected_at);

CREATE INDEX IF NOT EXISTS idx_ws_user_sessions_last_seen
    ON websocket.user_sessions (last_seen_at);
