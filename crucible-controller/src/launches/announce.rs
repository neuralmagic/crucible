//! Postgres `NOTIFY` wakes for work any replica can commit but only the leader acts on.

use sqlx::PgPool;

use crate::daemon::queue::WakeStream;

/// How long a failed listen waits before it tries again.
const LISTEN_RETRY: std::time::Duration = std::time::Duration::from_secs(5);

/// Yields when any of `channels` is notified. It yields once on connecting and once after every
/// reconnect as well, since a notification sent while it was not listening is never sent again.
/// Nothing connects until the stream is first polled.
pub fn wakes(pool: PgPool, channels: &'static [&'static str]) -> WakeStream {
    Box::pin(futures_util::stream::unfold(
        (pool, None::<sqlx::postgres::PgListener>),
        move |(pool, listener)| async move {
            let Some(mut listener) = listener else {
                let listener = listen(&pool, channels).await;
                return Some(((), (pool, Some(listener))));
            };
            match listener.recv().await {
                Ok(_) => while listener.next_buffered().is_some() {},
                Err(e) => {
                    tracing::warn!(error = %e, "launches: wake listener lost its connection");
                    tokio::time::sleep(LISTEN_RETRY).await;
                }
            }
            Some(((), (pool, Some(listener))))
        },
    ))
}

async fn listen(pool: &PgPool, channels: &[&str]) -> sqlx::postgres::PgListener {
    loop {
        let connected = async {
            let mut listener = sqlx::postgres::PgListener::connect_with(pool).await?;
            listener.listen_all(channels.iter().copied()).await?;
            Ok::<_, sqlx::Error>(listener)
        };
        match connected.await {
            Ok(listener) => return listener,
            Err(e) => {
                tracing::warn!(error = %e, "launches: cannot listen for wakes");
                tokio::time::sleep(LISTEN_RETRY).await;
            }
        }
    }
}
