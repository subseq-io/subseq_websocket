ALTER TABLE websocket.user_sessions
    ADD COLUMN IF NOT EXISTS expires_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_ws_user_sessions_expires_at
    ON websocket.user_sessions (expires_at);

UPDATE websocket.user_sessions
SET expires_at = NOW()
WHERE user_id IS NULL
  AND disconnected_at IS NOT NULL
  AND expires_at IS NULL;
