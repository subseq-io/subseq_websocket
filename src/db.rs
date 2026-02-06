use anyhow::anyhow;
use once_cell::sync::Lazy;
use sqlx::PgPool;
use sqlx::migrate::{MigrateError, Migrator};
use subseq_auth::user_id::UserId;
use uuid::Uuid;

use crate::error::{LibError, Result};
use crate::models::{ConnectionMetadata, WsConnection, WsUserSession};

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

/// Open or resume a websocket session.
///
/// - Authenticated sessions (`Some(user_id)`) are merged by `user_id`.
/// - Anonymous sessions (`None`) can be resumed by `session_id` if still valid.
pub async fn open_or_resume_session(
    pool: &PgPool,
    user_id: Option<UserId>,
    requested_session_id: Option<Uuid>,
    metadata: &ConnectionMetadata,
) -> Result<WsUserSession> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| LibError::database("failed to begin websocket transaction", e))?;

    purge_expired_anonymous_sessions_tx(&mut tx).await?;

    let metadata_json = metadata.as_json();
    let session = match user_id {
        Some(user_id) => upsert_user_session_tx(&mut tx, user_id, metadata_json)
            .await
            .map_err(|e| LibError::database("failed to open authenticated websocket session", e))?,
        None => {
            if let Some(session_id) = requested_session_id {
                if let Some(session) =
                    resume_anonymous_session_tx(&mut tx, session_id, &metadata_json)
                        .await
                        .map_err(|e| {
                            LibError::database("failed to resume anonymous websocket session", e)
                        })?
                {
                    session
                } else {
                    create_anonymous_session_tx(&mut tx, metadata_json)
                        .await
                        .map_err(|e| {
                            LibError::database("failed to create anonymous websocket session", e)
                        })?
                }
            } else {
                create_anonymous_session_tx(&mut tx, metadata_json)
                    .await
                    .map_err(|e| {
                        LibError::database("failed to create anonymous websocket session", e)
                    })?
            }
        }
    };

    tx.commit()
        .await
        .map_err(|e| LibError::database("failed to commit websocket open transaction", e))?;

    Ok(session)
}

/// Register a physical websocket connection for a session.
pub async fn register_connection(
    pool: &PgPool,
    session_id: Uuid,
    user_id: Option<UserId>,
    metadata: &ConnectionMetadata,
) -> Result<WsConnection> {
    let connection_id = Uuid::new_v4();
    let metadata_json = metadata.as_json();

    sqlx::query_as::<_, WsConnection>(
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
    .bind(user_id.map(|u| u.0))
    .bind(session_id)
    .bind(metadata_json)
    .fetch_one(pool)
    .await
    .map_err(|e| LibError::database("failed to register websocket connection", e))
}

/// Associate an existing websocket session with an authenticated user.
///
/// If another session already exists for that user, the source session is
/// merged into it.
pub async fn associate_user_with_session(
    pool: &PgPool,
    session_id: Uuid,
    user_id: UserId,
) -> Result<WsUserSession> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| LibError::database("failed to begin websocket association transaction", e))?;

    purge_expired_anonymous_sessions_tx(&mut tx).await?;

    let source_session = sqlx::query_as::<_, WsUserSession>(
        r#"
        SELECT
            session_id,
            user_id,
            connected_at,
            last_seen_at,
            disconnected_at,
            reconnect_count,
            metadata,
            expires_at
        FROM websocket.user_sessions
        WHERE session_id = $1
          AND (expires_at IS NULL OR expires_at > NOW())
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

    if let Some(existing_user_id) = source_session.user_id {
        if existing_user_id == user_id.0 {
            tx.commit().await.map_err(|e| {
                LibError::database("failed to commit websocket association transaction", e)
            })?;
            return Ok(source_session);
        }

        return Err(LibError::forbidden(
            "websocket session belongs to another user",
            anyhow!(
                "session_id {} already belongs to user {}",
                source_session.session_id,
                existing_user_id
            ),
        ));
    }

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
                    expires_at = NULL,
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
                    last_seen_at = NOW(),
                    expires_at = NULL
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
            metadata,
            expires_at
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

/// Mark a connection closed and update or expire its parent session.
///
/// Anonymous sessions (`user_id IS NULL`) are retained for
/// `anonymous_session_ttl_seconds` to support auth redirects.
pub async fn close_connection(
    pool: &PgPool,
    connection_id: Uuid,
    anonymous_session_ttl_seconds: i64,
) -> Result<()> {
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
                        last_seen_at = NOW(),
                        expires_at = NULL
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
                    UPDATE websocket.user_sessions
                    SET
                        disconnected_at = NOW(),
                        last_seen_at = NOW(),
                        expires_at = NOW() + ($2 * INTERVAL '1 second')
                    WHERE session_id = $1
                      AND user_id IS NULL
                    "#,
                )
                .bind(session_id)
                .bind(anonymous_session_ttl_seconds.max(1))
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    LibError::database("failed to set anonymous websocket session expiration", e)
                })?;
            }
        } else {
            sqlx::query(
                r#"
                UPDATE websocket.user_sessions
                SET
                    disconnected_at = NULL,
                    last_seen_at = NOW(),
                    expires_at = NULL
                WHERE session_id = $1
                "#,
            )
            .bind(session_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| LibError::database("failed to refresh websocket session heartbeat", e))?;
        }
    }

    purge_expired_anonymous_sessions_tx(&mut tx).await?;

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
            metadata,
            expires_at
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

/// Purge expired anonymous sessions.
pub async fn purge_expired_anonymous_sessions(pool: &PgPool) -> Result<u64> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| LibError::database("failed to begin websocket purge transaction", e))?;

    let deleted = purge_expired_anonymous_sessions_tx(&mut tx).await?;

    tx.commit()
        .await
        .map_err(|e| LibError::database("failed to commit websocket purge transaction", e))?;

    Ok(deleted)
}

async fn upsert_user_session_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: UserId,
    metadata: serde_json::Value,
) -> core::result::Result<WsUserSession, sqlx::Error> {
    sqlx::query_as::<_, WsUserSession>(
        r#"
        INSERT INTO websocket.user_sessions
            (session_id, user_id, connected_at, last_seen_at, disconnected_at, reconnect_count, metadata, expires_at)
        VALUES
            ($1, $2, NOW(), NOW(), NULL, 0, $3, NULL)
        ON CONFLICT (user_id) DO UPDATE
        SET
            disconnected_at = NULL,
            last_seen_at = NOW(),
            reconnect_count = websocket.user_sessions.reconnect_count + CASE
                WHEN websocket.user_sessions.disconnected_at IS NULL THEN 0
                ELSE 1
            END,
            metadata = websocket.user_sessions.metadata || EXCLUDED.metadata,
            expires_at = NULL
        RETURNING
            session_id,
            user_id,
            connected_at,
            last_seen_at,
            disconnected_at,
            reconnect_count,
            metadata,
            expires_at
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(user_id.0)
    .bind(metadata)
    .fetch_one(&mut **tx)
    .await
}

async fn resume_anonymous_session_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: Uuid,
    metadata: &serde_json::Value,
) -> core::result::Result<Option<WsUserSession>, sqlx::Error> {
    sqlx::query_as::<_, WsUserSession>(
        r#"
        UPDATE websocket.user_sessions
        SET
            disconnected_at = NULL,
            last_seen_at = NOW(),
            reconnect_count = websocket.user_sessions.reconnect_count + CASE
                WHEN websocket.user_sessions.disconnected_at IS NULL THEN 0
                ELSE 1
            END,
            metadata = websocket.user_sessions.metadata || $2::jsonb,
            expires_at = NULL
        WHERE session_id = $1
          AND user_id IS NULL
          AND (expires_at IS NULL OR expires_at > NOW())
        RETURNING
            session_id,
            user_id,
            connected_at,
            last_seen_at,
            disconnected_at,
            reconnect_count,
            metadata,
            expires_at
        "#,
    )
    .bind(session_id)
    .bind(metadata)
    .fetch_optional(&mut **tx)
    .await
}

async fn create_anonymous_session_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    metadata: serde_json::Value,
) -> core::result::Result<WsUserSession, sqlx::Error> {
    sqlx::query_as::<_, WsUserSession>(
        r#"
        INSERT INTO websocket.user_sessions
            (session_id, user_id, connected_at, last_seen_at, disconnected_at, reconnect_count, metadata, expires_at)
        VALUES
            ($1, NULL, NOW(), NOW(), NULL, 0, $2, NULL)
        RETURNING
            session_id,
            user_id,
            connected_at,
            last_seen_at,
            disconnected_at,
            reconnect_count,
            metadata,
            expires_at
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(metadata)
    .fetch_one(&mut **tx)
    .await
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

async fn purge_expired_anonymous_sessions_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<u64> {
    sqlx::query(
        r#"
        DELETE FROM websocket.user_sessions
        WHERE user_id IS NULL
          AND expires_at IS NOT NULL
          AND expires_at <= NOW()
        "#,
    )
    .execute(&mut **tx)
    .await
    .map(|res| res.rows_affected())
    .map_err(|e| LibError::database("failed to purge expired anonymous websocket sessions", e))
}
