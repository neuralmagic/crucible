use crate::api::dto::*;
use crate::api::state::*;
use axum::extract::{Query, State};
use serde::Deserialize;
use utoipa::ToSchema;

/// How many events `GET /api/events` (and the `/activity` page) return by default, and the most
/// a `limit=` can ask for — the feed is a tail, not an export (the NDJSON file is the export).
const EVENTS_TAIL_DEFAULT: usize = 50;

const EVENTS_TAIL_MAX: usize = 500;

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct EventsQuery {
    limit: Option<usize>,
}

/// `GET /api/events?limit=N` — the `N` most recent transitions across every issue, newest first.
#[utoipa::path(
    get,
    path = "/api/events",
    params(
        ("limit" = Option<usize>, Query, description = "Maximum events to return (default 50, max 500)")
    ),
    responses(
        (status = 200, description = "Recent status transitions across all issues (`reason` is truncated; the full text is on the issue detail)", body = Vec<EventDto>)
    )
)]
pub(crate) async fn list_events(
    State(state): State<ApiState>,
    Query(q): Query<EventsQuery>,
) -> Result<Json<Vec<EventDto>>, AppError> {
    let limit = q.limit.unwrap_or(EVENTS_TAIL_DEFAULT).min(EVENTS_TAIL_MAX);
    let recent = state.db.events().read_recent(limit).await?;
    Ok(Json(
        recent
            .into_iter()
            .map(|e| EventDto::from(e).truncated_for_list())
            .collect(),
    ))
}

/// `GET /api/events/stream` — the live feed as SSE, one JSON [`EventDto`] per transition from
/// subscription time on (history is `GET /api/events`). The frames come off the events *table*,
/// so transitions written by another replica appear too; the stream ends only when the daemon
/// shuts down.
///
/// Every frame carries the event's table id as its SSE id, so a dropped connection resumes
/// where it left off: the browser replays `Last-Event-ID` on reconnect, and an explicit
/// `?after=<id>` does the same for hand-rolled clients. Failover to a standby resumes the same
/// way — the id is a table id, not a process-local counter.
#[utoipa::path(
    get,
    path = "/api/events/stream",
    params(
        ("after" = Option<i64>, Query, description = "Resume after this event id; the Last-Event-ID header takes precedence")
    ),
    responses(
        (status = 200, description = "Server-sent event stream of real-time status transitions", content_type = "text/event-stream")
    )
)]
pub(crate) async fn events_stream(
    State(state): State<ApiState>,
    Query(resume): Query<EventsResumeQuery>,
    headers: axum::http::HeaderMap,
) -> Result<
    axum::response::sse::Sse<
        impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
    >,
    AppError,
> {
    use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
    use futures_util::StreamExt;

    let last_event_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<i64>().ok());
    let start = match last_event_id.or(resume.after) {
        Some(after) => after,
        None => state.db.events().latest_id().await?,
    };
    let stream = state
        .db
        .events()
        .tail_after(start)
        .filter_map(|(id, rec)| async move {
            // A plain DTO can't fail to serialize; skip the frame rather than kill the feed.
            SseEvent::default()
                .id(id.to_string())
                .json_data(EventDto::from(rec).truncated_for_list())
                .ok()
                .map(Ok)
        });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// The resume handle for [`events_stream`]: events with `id > after` replay first.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct EventsResumeQuery {
    #[serde(default)]
    after: Option<i64>,
}
