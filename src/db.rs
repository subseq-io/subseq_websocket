use anyhow::anyhow;
use once_cell::sync::Lazy;
use sqlx::PgPool;
use sqlx::migrate::{MigrateError, Migrator};
use uuid::Uuid;

use crate::error::{LibError, Result};
use crate::models::{ConnectionLease, ConnectionMetadata, WsConnection, WsUserSession};

/// SQLx migrator for websocket session tables.
pub static MIGRATOR: Lazy<Migrator> = Lazy::new(|| {
    let mut migrator = sqlx::migrate!("./migrations");
    migrator.set_ignore_missing(true);
    migrator
});

/// Create or upgrade websocket persistence tables.
pub async fn create_websocket_tables(pool: &PgPool) -> core::result::Result<(), MigrateError> {
    MIGRATOR.run(pool).await
}

/// Open or resume a user-level websocket session and register a new connection.
///
/// Session rows are keyed by `user_id`, allowing multiple tabs to share one
/// logical session while still tracking each physical connection separately.
pub async fn open_connection(
    pool: &PgPool,
    user_id: Uuid,
    metadata: &ConnectionMetadata,
) -> Result<ConnectionLease> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| LibError::database("failed to begin websocket transaction", e))?;

    let next_session_id = Uuid::new_v4();
    let metadata_json = metadata.as_json();

    let session = sqlx::query_as::<_, WsUserSession>(
        r#"
        INSERT INTO websocket.user_sessions
            (user_id, session_id, connected_at, last_seen_at, disconnected_at, reconnect_count, metadata)
        VALUES
            ($1, $2, NOW(), NOW(), NULL, 0, $3)
        ON CONFLICT (user_id) DO UPDATE
        SET
            last_seen_at = NOW(),
            disconnected_at = NULL,
            reconnect_count = websocket.user_sessions.reconnect_count + CASE
                WHEN websocket.user_sessions.disconnected_at IS NULL THEN 0
                ELSE 1
            END,
            metadata = websocket.user_sessions.metadata || EXCLUDED.metadata
        RETURNING
            user_id,
            session_id,
            connected_at,
            last_seen_at,
            disconnected_at,
            reconnect_count,
            metadata
        "#,
    )
    .bind(user_id)
    .bind(next_session_id)
    .bind(metadata_json.clone())
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| LibError::database("failed to open user websocket session", e))?;

    let connection_id = Uuid::new_v4();
    let connection = sqlx::query_as::<_, WsConnection>(
        r#"
        INSERT INTO websocket.connections
            (connection_id, user_id, session_id, connected_at, last_seen_at, disconnected_at, metadata)
        VALUES
            ($1, $2, $3, NOW(), NOW(), NULL, $4)
        RETURNING
            connection_id,
            user_id,
            session_id,
            connected_at,
            last_seen_at,
            disconnected_at,
            metadata
        "#,
    )
    .bind(connection_id)
    .bind(user_id)
    .bind(session.session_id)
    .bind(metadata_json)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| LibError::database("failed to register websocket connection", e))?;

    tx.commit()
        .await
        .map_err(|e| LibError::database("failed to commit websocket open transaction", e))?;

    Ok(ConnectionLease {
        session,
        connection,
    })
}

/// Refresh heartbeat timestamps for an active connection.
pub async fn touch_connection(pool: &PgPool, connection_id: Uuid) -> Result<()> {
    sqlx::query(
        r#"
        WITH touched AS (
            UPDATE websocket.connections
            SET last_seen_at = NOW()
            WHERE connection_id = $1 AND disconnected_at IS NULL
            RETURNING user_id
        )
        UPDATE websocket.user_sessions
        SET last_seen_at = NOW()
        WHERE user_id IN (SELECT user_id FROM touched)
        "#,
    )
    .bind(connection_id)
    .execute(pool)
    .await
    .map_err(|e| LibError::database("failed to touch websocket connection", e))?;

    Ok(())
}

/// Mark a connection closed and update user-session disconnect state when
/// the final active connection for that user goes away.
pub async fn close_connection(pool: &PgPool, connection_id: Uuid) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| LibError::database("failed to begin websocket close transaction", e))?;

    let user_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        UPDATE websocket.connections
        SET
            disconnected_at = COALESCE(disconnected_at, NOW()),
            last_seen_at = NOW()
        WHERE connection_id = $1
        RETURNING user_id
        "#,
    )
    .bind(connection_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| LibError::database("failed to close websocket connection", e))?;

    if let Some(user_id) = user_id {
        let active_count = active_connection_count_tx(&mut tx, user_id).await?;

        if active_count == 0 {
            sqlx::query(
                r#"
                UPDATE websocket.user_sessions
                SET
                    disconnected_at = NOW(),
                    last_seen_at = NOW()
                WHERE user_id = $1
                "#,
            )
            .bind(user_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                LibError::database("failed to mark websocket session as disconnected", e)
            })?;
        } else {
            sqlx::query(
                r#"
                UPDATE websocket.user_sessions
                SET
                    disconnected_at = NULL,
                    last_seen_at = NOW()
                WHERE user_id = $1
                "#,
            )
            .bind(user_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| LibError::database("failed to refresh websocket session heartbeat", e))?;
        }
    }

    tx.commit()
        .await
        .map_err(|e| LibError::database("failed to commit websocket close transaction", e))?;

    Ok(())
}

/// Count currently active (not disconnected) websocket connections for a user.
pub async fn active_connection_count(pool: &PgPool, user_id: Uuid) -> Result<i64> {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)
        FROM websocket.connections
        WHERE user_id = $1 AND disconnected_at IS NULL
        "#,
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .map_err(|e| LibError::database("failed to count active websocket connections", e))
}

/// Fetch the current logical websocket session row for a user.
pub async fn get_user_session(pool: &PgPool, user_id: Uuid) -> Result<Option<WsUserSession>> {
    sqlx::query_as::<_, WsUserSession>(
        r#"
        SELECT
            user_id,
            session_id,
            connected_at,
            last_seen_at,
            disconnected_at,
            reconnect_count,
            metadata
        FROM websocket.user_sessions
        WHERE user_id = $1
        LIMIT 1
        "#,
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| LibError::database("failed to fetch websocket session", e))
}

async fn active_connection_count_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: Uuid,
) -> Result<i64> {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)
        FROM websocket.connections
        WHERE user_id = $1 AND disconnected_at IS NULL
        "#,
    )
    .bind(user_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| {
        LibError::database(
            "failed to count active websocket connections",
            anyhow!(e).context(format!("user_id={user_id}")),
        )
    })
}
