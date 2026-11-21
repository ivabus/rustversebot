use super::{BotState, PublicApiError};
use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, put},
};
use serde::Deserialize;
use std::sync::Arc;
use subtle::ConstantTimeEq;

pub(super) async fn index() -> Response {
    let html = Html(
        include_str!("../../web/admin.html")
            .replace("/*__STYLE__*/", include_str!("../../web/admin.css"))
            .replace("/*__SCRIPT__*/", include_str!("../../web/admin.js")),
    );
    (
        [
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        html,
    )
        .into_response()
}

pub(super) fn routes(token: Option<Arc<str>>) -> Router<Arc<BotState>> {
    Router::new()
        .route("/chats", get(chats))
        .route("/chats/{chat}/features/{feature}", put(set_feature))
        .route("/sessions", get(sessions))
        .route("/sessions/{chat}/{root}", get(turns))
        .route_layer(middleware::from_fn_with_state(token, authorize))
}

async fn authorize(State(token): State<Option<Arc<str>>>, req: Request, next: Next) -> Response {
    let supplied = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.as_bytes().strip_prefix(b"Bearer "));
    let mut response = match token {
        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        Some(token) if supplied.is_some_and(|v| bool::from(token.as_bytes().ct_eq(v))) => {
            next.run(req).await
        }
        Some(_) => StatusCode::UNAUTHORIZED.into_response(),
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}

async fn chats(State(state): State<Arc<BotState>>) -> Result<impl IntoResponse, PublicApiError> {
    Ok(Json(state.db.admin_chats().await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FeatureUpdate {
    enabled: bool,
}

async fn set_feature(
    State(state): State<Arc<BotState>>,
    Path((chat, feature)): Path<(i64, crate::db::AiFeature)>,
    Json(update): Json<FeatureUpdate>,
) -> Result<StatusCode, PublicApiError> {
    // Telegram chat identifiers have at most 52 significant bits.
    if chat == 0 || chat.unsigned_abs() >= 1 << 52 {
        return Err(PublicApiError::bad_request("invalid chat ID"));
    }
    state.db.set_feature(chat, feature, update.enabled).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default)]
struct SessionsQuery {
    chat: Option<i64>,
    #[serde(default)]
    page: u32,
}

async fn sessions(
    State(state): State<Arc<BotState>>,
    Query(q): Query<SessionsQuery>,
) -> Result<impl IntoResponse, PublicApiError> {
    const PAGE: i64 = 25;
    let mut items = state
        .db
        .admin_sessions(q.chat, i64::from(q.page) * PAGE, PAGE + 1)
        .await?;
    let more = items.len() > PAGE as usize;
    items.truncate(PAGE as usize);
    Ok(Json(serde_json::json!({"items":items,"more":more})))
}

async fn turns(
    State(state): State<Arc<BotState>>,
    Path((chat, root)): Path<(i64, i32)>,
) -> Result<Response, PublicApiError> {
    let turns = state.db.admin_turns(chat, root).await?;
    Ok(if turns.is_empty() {
        StatusCode::NOT_FOUND.into_response()
    } else {
        Json(turns).into_response()
    })
}
