use super::ids::*;
use crate::database::models::DatabaseError;
use crate::models::ids::base62_impl::{parse_base62, to_base62};
use chrono::{DateTime, Utc};
use redis::cmd;
use serde::{Deserialize, Serialize};

const SESSIONS_NAMESPACE: &str = "sessions";
const SESSIONS_IDS_NAMESPACE: &str = "sessions_ids";
const SESSIONS_USERS_NAMESPACE: &str = "sessions_users";
const DEFAULT_EXPIRY: i64 = 1800; // 30 minutes

pub struct SessionBuilder {
    pub session: String,
    pub user_id: UserId,

    pub os: Option<String>,
    pub platform: Option<String>,

    pub city: Option<String>,
    pub country: Option<String>,

    pub ip: String,
    pub user_agent: String,
}

impl SessionBuilder {
    pub async fn insert(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<SessionId, DatabaseError> {
        let id = generate_session_id(&mut *transaction).await?;

        sqlx::query!(
            "
            INSERT INTO sessions (
                id, session, user_id, os, platform,
                city, country, ip, user_agent
            )
            VALUES (
                $1, $2, $3, $4, $5,
                $6, $7, $8, $9
            )
            ",
            id as SessionId,
            self.session,
            self.user_id as UserId,
            self.os,
            self.platform,
            self.city,
            self.country,
            self.ip,
            self.user_agent,
        )
        .execute(&mut *transaction)
        .await?;

        Ok(id)
    }
}

#[derive(Deserialize, Serialize)]
pub struct Session {
    pub id: SessionId,
    pub session: String,
    pub user_id: UserId,

    pub created: DateTime<Utc>,
    pub last_login: DateTime<Utc>,
    pub expires: DateTime<Utc>,
    pub refresh_expires: DateTime<Utc>,

    pub os: Option<String>,
    pub platform: Option<String>,
    pub user_agent: String,

    pub city: Option<String>,
    pub country: Option<String>,
    pub ip: String,
}

impl Session {
    pub async fn get<'a, E, T: ToString>(
        id: T,
        exec: E,
        redis: &deadpool_redis::Pool,
    ) -> Result<Option<Session>, DatabaseError>
    where
        E: sqlx::Executor<'a, Database = sqlx::Postgres>,
    {
        Self::get_many(&[id], exec, redis)
            .await
            .map(|x| x.into_iter().next())
    }

    pub async fn get_id<'a, 'b, E>(
        id: SessionId,
        executor: E,
        redis: &deadpool_redis::Pool,
    ) -> Result<Option<Session>, DatabaseError>
    where
        E: sqlx::Executor<'a, Database = sqlx::Postgres>,
    {
        Session::get_many(&[crate::models::ids::SessionId::from(id)], executor, redis)
            .await
            .map(|x| x.into_iter().next())
    }

    pub async fn get_many_ids<'a, E>(
        session_ids: &[SessionId],
        exec: E,
        redis: &deadpool_redis::Pool,
    ) -> Result<Vec<Session>, DatabaseError>
    where
        E: sqlx::Executor<'a, Database = sqlx::Postgres>,
    {
        let ids = session_ids
            .iter()
            .map(|x| crate::models::ids::SessionId::from(*x))
            .collect::<Vec<_>>();
        Session::get_many(&ids, exec, redis).await
    }

    pub async fn get_many<'a, E, T: ToString>(
        session_strings: &[T],
        exec: E,
        redis: &deadpool_redis::Pool,
    ) -> Result<Vec<Session>, DatabaseError>
    where
        E: sqlx::Executor<'a, Database = sqlx::Postgres>,
    {
        use futures::TryStreamExt;

        if session_strings.is_empty() {
            return Ok(Vec::new());
        }

        let mut redis = redis.get().await?;

        let mut found_sessions = Vec::new();
        let mut remaining_strings = session_strings
            .iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>();

        let mut session_ids = session_strings
            .iter()
            .flat_map(|x| parse_base62(&x.to_string()).map(|x| x as i64))
            .collect::<Vec<_>>();

        session_ids.append(
            &mut cmd("MGET")
                .arg(
                    session_strings
                        .iter()
                        .map(|x| format!("{}:{}", SESSIONS_IDS_NAMESPACE, x.to_string()))
                        .collect::<Vec<_>>(),
                )
                .query_async::<_, Vec<Option<i64>>>(&mut redis)
                .await?
                .into_iter()
                .flatten()
                .collect(),
        );

        if !session_ids.is_empty() {
            let sessions = cmd("MGET")
                .arg(
                    session_ids
                        .iter()
                        .map(|x| format!("{}:{}", SESSIONS_NAMESPACE, x))
                        .collect::<Vec<_>>(),
                )
                .query_async::<_, Vec<Option<String>>>(&mut redis)
                .await?;

            for session in sessions {
                if let Some(session) =
                    session.and_then(|x| serde_json::from_str::<Session>(&x).ok())
                {
                    remaining_strings
                        .retain(|x| &to_base62(session.id.0 as u64) != x && &session.session != x);
                    found_sessions.push(session);
                    continue;
                }
            }
        }

        if !remaining_strings.is_empty() {
            let session_ids_parsed: Vec<i64> = remaining_strings
                .iter()
                .flat_map(|x| parse_base62(&x.to_string()).ok())
                .map(|x| x as i64)
                .collect();
            let db_sessions: Vec<Session> = sqlx::query!(
                "
                SELECT id, user_id, session, created, last_login, expires, refresh_expires, os, platform,
                city, country, ip, user_agent
                FROM sessions
                WHERE id = ANY($1) OR session = ANY($2)
                ORDER BY created DESC
                ",
                &session_ids_parsed,
                &remaining_strings.into_iter().map(|x| x.to_string()).collect::<Vec<_>>(),
            )
                .fetch_many(exec)
                .try_filter_map(|e| async {
                    Ok(e.right().map(|x| Session {
                        id: SessionId(x.id),
                        session: x.session,
                        user_id: UserId(x.user_id),
                        created: x.created,
                        last_login: x.last_login,
                        expires: x.expires,
                        refresh_expires: x.refresh_expires,
                        os: x.os,
                        platform: x.platform,
                        city: x.city,
                        country: x.country,
                        ip: x.ip,
                        user_agent: x.user_agent,
                    }))
                })
                .try_collect::<Vec<Session>>()
                .await?;

            for session in db_sessions {
                cmd("SET")
                    .arg(format!("{}:{}", SESSIONS_NAMESPACE, session.id.0))
                    .arg(serde_json::to_string(&session)?)
                    .arg("EX")
                    .arg(DEFAULT_EXPIRY)
                    .query_async::<_, ()>(&mut redis)
                    .await?;

                cmd("SET")
                    .arg(format!("{}:{}", SESSIONS_IDS_NAMESPACE, session.session))
                    .arg(session.id.0)
                    .arg("EX")
                    .arg(DEFAULT_EXPIRY)
                    .query_async::<_, ()>(&mut redis)
                    .await?;
                found_sessions.push(session);
            }
        }

        Ok(found_sessions)
    }

    pub async fn get_user_sessions<'a, E>(
        user_id: UserId,
        exec: E,
        redis: &deadpool_redis::Pool,
    ) -> Result<Vec<SessionId>, DatabaseError>
    where
        E: sqlx::Executor<'a, Database = sqlx::Postgres>,
    {
        let mut redis = redis.get().await?;
        let res = cmd("GET")
            .arg(format!("{}:{}", SESSIONS_USERS_NAMESPACE, user_id.0))
            .query_async::<_, Option<String>>(&mut redis)
            .await?
            .and_then(|x| serde_json::from_str::<Vec<i64>>(&x).ok());

        if let Some(res) = res {
            return Ok(res.into_iter().map(SessionId).collect());
        }

        use futures::TryStreamExt;
        let db_sessions: Vec<SessionId> = sqlx::query!(
            "
                SELECT id
                FROM sessions
                WHERE user_id = $1
                ORDER BY created DESC
                ",
            user_id.0,
        )
        .fetch_many(exec)
        .try_filter_map(|e| async { Ok(e.right().map(|x| SessionId(x.id))) })
        .try_collect::<Vec<SessionId>>()
        .await?;

        cmd("SET")
            .arg(format!("{}:{}", SESSIONS_USERS_NAMESPACE, user_id.0))
            .arg(serde_json::to_string(&db_sessions)?)
            .arg("EX")
            .arg(DEFAULT_EXPIRY)
            .query_async::<_, ()>(&mut redis)
            .await?;

        Ok(db_sessions)
    }

    pub async fn clear_cache(
        clear_sessions: Vec<(Option<SessionId>, Option<String>, Option<UserId>)>,
        redis: &deadpool_redis::Pool,
    ) -> Result<(), DatabaseError> {
        if clear_sessions.is_empty() {
            return Ok(());
        }

        let mut redis = redis.get().await?;
        let mut cmd = cmd("DEL");

        for (id, session, user_id) in clear_sessions {
            if let Some(id) = id {
                cmd.arg(format!("{}:{}", SESSIONS_NAMESPACE, id.0));
            }
            if let Some(session) = session {
                cmd.arg(format!("{}:{}", SESSIONS_IDS_NAMESPACE, session));
            }
            if let Some(user_id) = user_id {
                cmd.arg(format!("{}:{}", SESSIONS_USERS_NAMESPACE, user_id.0));
            }
        }

        cmd.query_async::<_, ()>(&mut redis).await?;

        Ok(())
    }

    pub async fn remove(
        id: SessionId,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<Option<()>, sqlx::error::Error> {
        sqlx::query!(
            "
            DELETE FROM sessions WHERE id = $1
            ",
            id as SessionId,
        )
        .execute(&mut *transaction)
        .await?;

        Ok(Some(()))
    }
}
