use anyhow::anyhow;
use once_cell::sync::Lazy;
use sqlx::PgPool;
use sqlx::migrate::{MigrateError, Migrator};
use subseq_auth::user_id::UserId;
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

/// Open or resume a websocket session and register a new connection.
///
/// - Authenticated sessions (`Some(user_id)`) are merged by `user_id`.
/// - Anonymous sessions (`None`) are always created with a fresh `session_id`.
pub async fn open_connection(
    pool: &PgPool,
    user_id: Option<UserId>,
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
            (session_id, user_id, connected_at, last_seen_at, disconnected_at, reconnect_count, metadata)
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
            session_id,
            user_id,
            connected_at,
            last_seen_at,
            disconnected_at,
            reconnect_count,
            metadata
        "#,
    )
    .bind(next_session_id)
    .bind(user_id.map(|u| u.0))
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
    .bind(session.user_id)
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

/// Associate an existing active session with an authenticated user.
///
/// If another session already exists for that `user_id`, connections are moved
/// onto the existing session and the source anonymous session is removed.
pub async fn associate_user_with_session(
    pool: &PgPool,
    session_id: Uuid,
    user_id: UserId,
) -> Result<WsUserSession> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| LibError::database("failed to begin websocket association transaction", e))?;

    let source_session = sqlx::query_as::<_, WsUserSession>(
        r#"
        SELECT
            session_id,
            user_id,
            connected_at,
            last_seen_at,
            disconnected_at,
            reconnect_count,
            metadata
        FROM websocket.user_sessions
        WHERE session_id = $1
        LIMIT 1
        FOR UPDATE
        "#,
    )
    .bind(session_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| LibError::database("failed to fetch source websocket session", e))?
    .ok_or_else(|| {
        LibError::not_found(
            "websocket session not found",
            anyhow!("session_id {session_id} was not found"),
        )
    })?;

    let existing_user_session_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT session_id
        FROM websocket.user_sessions
        WHERE user_id = $1
        LIMIT 1
        FOR UPDATE
        "#,
    )
    .bind(user_id.0)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| LibError::database("failed to look up user websocket session", e))?;

    let final_session_id = match existing_user_session_id {
        Some(target_session_id) if target_session_id != source_session.session_id => {
            sqlx::query(
                r#"
                UPDATE websocket.connections
                SET
                    session_id = $1,
                    user_id = $2
                WHERE session_id = $3
                "#,
            )
            .bind(target_session_id)
            .bind(user_id.0)
            .bind(source_session.session_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                LibError::database("failed to merge websocket connections into user session", e)
            })?;

            sqlx::query(
                r#"
                UPDATE websocket.user_sessions
                SET
                    disconnected_at = NULL,
                    last_seen_at = NOW(),
                    metadata = websocket.user_sessions.metadata || $2::jsonb
                WHERE session_id = $1
                "#,
            )
            .bind(target_session_id)
            .bind(source_session.metadata.clone())
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                LibError::database("failed to refresh merged websocket user session", e)
            })?;

            sqlx::query(
                r#"
                DELETE FROM websocket.user_sessions
                WHERE session_id = $1
                "#,
            )
            .bind(source_session.session_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                LibError::database("failed to remove merged source websocket session", e)
            })?;

            target_session_id
        }
        _ => {
            sqlx::query(
                r#"
                UPDATE websocket.user_sessions
                SET
                    user_id = $1,
                    disconnected_at = NULL,
                    last_seen_at = NOW()
                WHERE session_id = $2
                "#,
            )
            .bind(user_id.0)
            .bind(source_session.session_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| LibError::database("failed to associate websocket session user", e))?;

            sqlx::query(
                r#"
                UPDATE websocket.connections
                SET user_id = $1
                WHERE session_id = $2
                "#,
            )
            .bind(user_id.0)
            .bind(source_session.session_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                LibError::database("failed to update websocket connection user association", e)
            })?;

            source_session.session_id
        }
    };

    let session = sqlx::query_as::<_, WsUserSession>(
        r#"
        SELECT
            session_id,
            user_id,
            connected_at,
            last_seen_at,
            disconnected_at,
            reconnect_count,
            metadata
        FROM websocket.user_sessions
        WHERE session_id = $1
        LIMIT 1
        "#,
    )
    .bind(final_session_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| LibError::database("failed to fetch associated websocket session", e))?;

    tx.commit()
        .await
        .map_err(|e| LibError::database("failed to commit websocket association transaction", e))?;

    Ok(session)
}

/// Refresh heartbeat timestamps for an active connection.
pub async fn touch_connection(pool: &PgPool, connection_id: Uuid) -> Result<()> {
    sqlx::query(
        r#"
        WITH touched AS (
            UPDATE websocket.connections
            SET last_seen_at = NOW()
            WHERE connection_id = $1 AND disconnected_at IS NULL
            RETURNING session_id
        )
        UPDATE websocket.user_sessions
        SET last_seen_at = NOW()
        WHERE session_id IN (SELECT session_id FROM touched)
        "#,
    )
    .bind(connection_id)
    .execute(pool)
    .await
    .map_err(|e| LibError::database("failed to touch websocket connection", e))?;

    Ok(())
}

/// Mark a connection closed and update or remove its parent session.
///
/// Anonymous sessions (`user_id IS NULL`) are deleted once their final
/// connection closes.
pub async fn close_connection(pool: &PgPool, connection_id: Uuid) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| LibError::database("failed to begin websocket close transaction", e))?;

    let session_and_user = sqlx::query_as::<_, (Uuid, Option<Uuid>)>(
        r#"
        UPDATE websocket.connections
        SET
            disconnected_at = COALESCE(disconnected_at, NOW()),
            last_seen_at = NOW()
        WHERE connection_id = $1
        RETURNING session_id, user_id
        "#,
    )
    .bind(connection_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| LibError::database("failed to close websocket connection", e))?;

    if let Some((session_id, session_user_id)) = session_and_user {
        let active_count = active_connection_count_for_session_tx(&mut tx, session_id).await?;

        if active_count == 0 {
            if session_user_id.is_some() {
                sqlx::query(
                    r#"
                    UPDATE websocket.user_sessions
                    SET
                        disconnected_at = NOW(),
                        last_seen_at = NOW()
                    WHERE session_id = $1
                    "#,
                )
                .bind(session_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    LibError::database("failed to mark websocket session as disconnected", e)
                })?;
            } else {
                sqlx::query(
                    r#"
                    DELETE FROM websocket.user_sessions
                    WHERE session_id = $1
                      AND user_id IS NULL
                    "#,
                )
                .bind(session_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    LibError::database("failed to delete anonymous websocket session", e)
                })?;
            }
        } else {
            sqlx::query(
                r#"
                UPDATE websocket.user_sessions
                SET
                    disconnected_at = NULL,
                    last_seen_at = NOW()
                WHERE session_id = $1
                "#,
            )
            .bind(session_id)
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

/// Count currently active (not disconnected) websocket connections for a session.
pub async fn active_connection_count_for_session(pool: &PgPool, session_id: Uuid) -> Result<i64> {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)
        FROM websocket.connections
        WHERE session_id = $1 AND disconnected_at IS NULL
        "#,
    )
    .bind(session_id)
    .fetch_one(pool)
    .await
    .map_err(|e| LibError::database("failed to count active websocket connections", e))
}

/// Fetch the current logical websocket session row for a user.
pub async fn get_user_session(pool: &PgPool, user_id: Uuid) -> Result<Option<WsUserSession>> {
    sqlx::query_as::<_, WsUserSession>(
        r#"
        SELECT
            session_id,
            user_id,
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

async fn active_connection_count_for_session_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: Uuid,
) -> Result<i64> {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)
        FROM websocket.connections
        WHERE session_id = $1 AND disconnected_at IS NULL
        "#,
    )
    .bind(session_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| {
        LibError::database(
            "failed to count active websocket connections",
            anyhow!(e).context(format!("session_id={session_id}")),
        )
    })
}
