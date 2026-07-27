//! A small, read-only HTTP dashboard.
//!
//! The caller decides whether to start the dashboard.
//! Bind to a loopback address unless the operator selects a public listener.
//! For example, use `127.0.0.1:8080` for a loopback listener.

use crate::BotState;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use serde::Serialize;
use std::{net::SocketAddr, sync::Arc};

const INDEX: &str = include_str!("../web/index.html");
const STYLE: &str = include_str!("../web/style.css");
const SCRIPT: &str = include_str!("../web/app.js");
const MAX_HISTORY_ENTRIES: usize = 200;

/// Start serving the dashboard until the task is cancelled.
pub async fn serve(
    state: Arc<BotState>,
    bind: SocketAddr,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    log::info!("Web dashboard listening on http://{bind}");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            while !*shutdown.borrow() {
                if shutdown.changed().await.is_err() {
                    break;
                }
            }
        })
        .await?;
    Ok(())
}

pub fn router(state: Arc<BotState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(health))
        .route("/api/chats", get(chats))
        .route("/api/chats/{chat_id}/leaderboard/{kind}", get(leaderboard))
        .route("/api/users/{uid}/history", get(history))
        .fallback(not_found)
        .with_state(state)
}

async fn index() -> Html<String> {
    Html(
        INDEX
            .replace("/*__STYLE__*/", STYLE)
            .replace("/*__SCRIPT__*/", SCRIPT),
    )
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

async fn health() -> Json<Health> {
    Json(Health { status: "ok" })
}

async fn chats(State(state): State<Arc<BotState>>) -> Result<impl IntoResponse, PublicApiError> {
    Ok(Json(state.db.web_chats().await?))
}

async fn leaderboard(
    State(state): State<Arc<BotState>>,
    Path((chat_id, kind)): Path<(i64, String)>,
) -> Result<impl IntoResponse, PublicApiError> {
    let kind = validated_kind(&kind)?;
    Ok(Json(state.db.web_leaderboard(chat_id, kind).await?))
}

async fn history(
    State(state): State<Arc<BotState>>,
    Path(uid): Path<String>,
) -> Result<impl IntoResponse, PublicApiError> {
    validate_uid(&uid)?;
    Ok(Json(state.db.web_history(&uid, MAX_HISTORY_ENTRIES).await?))
}

fn validated_kind(kind: &str) -> Result<&str, PublicApiError> {
    match kind {
        "deadly_assault" | "shiyu_defense" => Ok(kind),
        _ => Err(PublicApiError::bad_request("unknown leaderboard type")),
    }
}

fn validate_uid(uid: &str) -> Result<(), PublicApiError> {
    if (6..=12).contains(&uid.len()) && uid.bytes().all(|byte| byte.is_ascii_digit()) {
        Ok(())
    } else {
        Err(PublicApiError::bad_request("invalid UID"))
    }
}

async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"error":"not found"}"#,
    )
        .into_response()
}

struct PublicApiError {
    status: StatusCode,
    public_message: &'static str,
    source: Option<anyhow::Error>,
}

impl PublicApiError {
    fn bad_request(message: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            public_message: message,
            source: None,
        }
    }
}

impl From<anyhow::Error> for PublicApiError {
    fn from(source: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            public_message: "database unavailable",
            source: Some(source),
        }
    }
}

impl IntoResponse for PublicApiError {
    fn into_response(self) -> Response {
        if let Some(source) = self.source {
            log::error!("Web API request failed: {source:#}");
        }
        (
            self.status,
            Json(serde_json::json!({ "error": self.public_message })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;

    async fn test_state() -> Arc<BotState> {
        Arc::new(BotState {
            db: crate::db::Db::new_test().await.unwrap(),
            templates: crate::templates::TemplateEngine::new().unwrap(),
            admin_id: 1,
            public_web_url: None,
            nanoka: nanoka::NanokaClient::new().with_lang("en"),
        })
    }

    #[test]
    fn only_known_leaderboard_types_are_accepted() {
        assert!(validated_kind("deadly_assault").is_ok());
        assert!(validated_kind("shiyu_defense").is_ok());
        assert!(validated_kind("../cookies").is_err());
    }

    #[test]
    fn uid_is_strictly_numeric_and_bounded() {
        assert!(validate_uid("123456789").is_ok());
        assert!(validate_uid("12345").is_err());
        assert!(validate_uid("1234567890123").is_err());
        assert!(validate_uid("12345<script>").is_err());
    }

    #[tokio::test]
    async fn dashboard_is_self_contained_and_accessible() {
        let Html(page) = index().await;
        assert!(page.contains("<meta name=\"viewport\""));
        assert!(page.contains("aria-live=\"polite\""));
        assert!(page.contains("<style>:root"));
        assert!(page.contains("<script>const $="));
        assert!(!page.contains("/*__STYLE__*/"));
        assert!(!page.contains("/*__SCRIPT__*/"));
        assert!(!page.contains("https://"));
        assert!(page.contains("new URLSearchParams(location.search)"));
        assert!(page.contains("validUid"));
        assert!(page.contains("validKind"));
    }

    #[tokio::test]
    async fn routes_return_expected_statuses_and_reject_invalid_paths() {
        let app = router(test_state().await);
        for (uri, expected) in [
            (
                "/?chat=10&uid=123456789&kind=deadly_assault",
                StatusCode::OK,
            ),
            ("/healthz", StatusCode::OK),
            ("/api/chats", StatusCode::OK),
            ("/api/chats/10/leaderboard/deadly_assault", StatusCode::OK),
            (
                "/api/chats/10/leaderboard/not-a-kind",
                StatusCode::BAD_REQUEST,
            ),
            ("/api/users/12345/history", StatusCode::BAD_REQUEST),
            ("/api/users/123456789/history", StatusCode::OK),
            ("/missing", StatusCode::NOT_FOUND),
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "unexpected status for {uri}");
        }
    }

    #[tokio::test]
    async fn public_responses_do_not_leak_cookie_or_admin_id() {
        let state = test_state().await;
        state.db.set_cookie("super-secret-cookie").await.unwrap();
        let app = router(state);

        for uri in ["/", "/healthz", "/api/chats", "/missing"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            let body = to_bytes(response.into_body(), 1_000_000).await.unwrap();
            let body = String::from_utf8_lossy(&body);
            assert!(!body.contains("super-secret-cookie"));
            assert!(!body.contains("\"admin_id\":1"));
        }
    }
}
