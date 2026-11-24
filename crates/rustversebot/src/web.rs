//! Public player statistics and a token-protected operator console.
//!
//! The caller decides whether to start the dashboard.
//! Bind to a loopback address unless the operator selects a public listener.
//! For example, use `127.0.0.1:8080` for a loopback listener.

use crate::BotState;
use axum::{
    Json, Router,
    extract::{MatchedPath, Path, Request, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use serde::Serialize;
use std::{net::SocketAddr, sync::Arc};
use tracing::Instrument;

const INDEX: &str = include_str!("../web/index.html");
const STYLE: &str = concat!(
    include_str!("../web/admin.css"),
    "\n",
    include_str!("../web/style.css")
);
const SCRIPT: &str = include_str!("../web/app.js");
const MAX_HISTORY_ENTRIES: usize = 200;
mod admin;

/// Start serving the dashboard until the task is cancelled.
pub async fn serve(
    state: Arc<BotState>,
    bind: SocketAddr,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    log::info!("Web dashboard listening on http://{bind}");
    let token = match std::env::var("BOT_WEB_ADMIN_TOKEN") {
        Ok(value) if value.trim().is_empty() => None,
        Ok(value) => {
            anyhow::ensure!(
                value.len() >= 32 && value.bytes().all(|b| b.is_ascii_graphic()),
                "BOT_WEB_ADMIN_TOKEN must contain at least 32 printable ASCII characters without spaces"
            );
            Some(Arc::from(value))
        }
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => return Err(error.into()),
    };
    axum::serve(listener, router(state, token))
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

fn router(state: Arc<BotState>, token: Option<Arc<str>>) -> Router {
    let admin_enabled = token.is_some();
    Router::new()
        .route(
            "/",
            get(move |uri: axum::http::Uri| async move {
                if admin_enabled && uri.query().is_none() {
                    admin::index().await
                } else {
                    index().await.into_response()
                }
            }),
        )
        .route("/players", get(index))
        .route("/admin", get(admin::index))
        .nest("/api/admin", admin::routes(token))
        .route("/healthz", get(health))
        .route("/api/chats", get(chats))
        .route("/api/chats/{chat_id}/leaderboard/{kind}", get(leaderboard))
        .route("/api/users/{uid}/history", get(history))
        .fallback(not_found)
        .layer(axum::middleware::from_fn(trace_request))
        .with_state(state)
}

async fn trace_request(req: Request, next: axum::middleware::Next) -> Response {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map_or("unmatched", MatchedPath::as_str);
    let method = match req.method().as_str() {
        method @ ("GET" | "PUT" | "HEAD" | "POST" | "DELETE" | "OPTIONS" | "PATCH" | "TRACE"
        | "CONNECT") => method,
        _ => "_OTHER",
    };
    let span = tracing::info_span!(target: "rustverse::bot", "http.request",
        otel.kind = "server", http.route = route, http.request.method = method,
        http.response.status_code = tracing::field::Empty, otel.status_code = tracing::field::Empty);
    async move {
        let response = next.run(req).await;
        let status = response.status();
        tracing::Span::current().record("http.response.status_code", status.as_u16());
        if status.is_server_error() {
            tracing::Span::current().record("otel.status_code", "ERROR");
        }
        response
    }
    .instrument(span)
    .await
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
    let zone = match rustverse::client::server::detect_server(&uid) {
        Some("prod_gf_eu") => Some(crate::season_time::EUROPE_SERVER),
        Some("prod_gf_us") => chrono::FixedOffset::west_opt(5 * 3600),
        Some("prod_gf_jp" | "prod_gf_sg") => chrono::FixedOffset::east_opt(8 * 3600),
        _ => None,
    };
    let history = state
        .db
        .web_history(&uid, MAX_HISTORY_ENTRIES)
        .await?
        .into_iter()
        .map(|entry| {
            let display =
                |value: &str| zone.map(|z| crate::season_time::display_wall_time(value, z));
            HistoryView {
                season_start_msk: display(&entry.season_start),
                season_end_msk: display(&entry.season_end),
                entry,
            }
        })
        .collect::<Vec<_>>();
    Ok(Json(history))
}

#[derive(Serialize)]
struct HistoryView {
    #[serde(flatten)]
    entry: crate::db::WebHistoryEntry,
    season_start_msk: Option<String>,
    season_end_msk: Option<String>,
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
            ai_jobs: crate::ai_jobs::channel().0,
            prompts: Default::default(),
            beta: crate::beta::BetaPages::new().unwrap(),
            db: crate::db::Db::new_test().await.unwrap(),
            templates: crate::templates::TemplateEngine::new().unwrap(),
            admin_id: 1,
            public_web_url: None,
            nanoka: nanoka::NanokaClient::new().with_lang("en"),
            http: reqwest::Client::new(),
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
        assert!(page.contains("history-form"));
        assert!(!page.contains("/*__STYLE__*/"));
        assert!(!page.contains("/*__SCRIPT__*/"));
        assert!(!page.contains("https://"));
        assert!(page.contains("new URLSearchParams(location.search)"));
        assert!(page.contains("validUid"));
        assert!(page.contains("validKind"));
    }

    #[tokio::test]
    async fn routes_return_expected_statuses_and_reject_invalid_paths() {
        let app = router(test_state().await, None);
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
    async fn web_spans_include_route_templates_and_status_without_request_data() {
        use tracing_subscriber::fmt::format::FmtSpan;
        let app = router(test_state().await, Some(Arc::from("admin-secret")));
        let file = tempfile::NamedTempFile::new().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_span_events(FmtSpan::CLOSE)
            .with_writer(file.reopen().unwrap())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        for path in [
            "/api/users/123456789/history?token=secret-query",
            "/api/admin/sessions/42/77",
            "/private-unknown",
        ] {
            admin_request(&app, "GET", path, Some("wrong-secret"), "").await;
        }
        let traces = std::fs::read_to_string(file.path()).unwrap();
        for expected in [
            "/api/users/{uid}/history",
            "/api/admin/sessions/{chat}/{root}",
            "401",
            "unmatched",
            "404",
        ] {
            assert!(traces.contains(expected), "missing {expected} in {traces}");
        }
        for private in [
            "123456789",
            "/42/77",
            "secret-query",
            "wrong-secret",
            "private-unknown",
        ] {
            assert!(!traces.contains(private));
        }
    }

    #[tokio::test]
    async fn history_exposes_moscow_dates_for_the_players_region() {
        let state = test_state().await;
        for (uid, expected) in [
            ("150000001", "06.11.2026 06:00:00 МСК"),
            ("130000001", "05.11.2026 23:00:00 МСК"),
        ] {
            state
                .db
                .insert_result(uid, "shiyu_defense", "2026-11-06 04:00:00", "", 1, "{}")
                .await
                .unwrap();
            let response = router(state.clone(), None)
                .oneshot(
                    Request::builder()
                        .uri(format!("/api/users/{uid}/history"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let body = to_bytes(response.into_body(), 4096).await.unwrap();
            let rows: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(rows[0]["season_start_msk"], expected);
            assert_eq!(rows[0]["season_start"], "2026-11-06 04:00:00");
        }
    }

    #[tokio::test]
    async fn public_responses_do_not_leak_cookie_or_admin_id() {
        let state = test_state().await;
        state.db.set_cookie("super-secret-cookie").await.unwrap();
        let app = router(state, None);

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

    async fn admin_request(
        app: &Router,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: &str,
    ) -> Response {
        let mut request = Request::builder()
            .uri(path)
            .method(method)
            .header("content-type", "application/json");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        app.clone()
            .oneshot(request.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn admin_routes_require_credentials_and_mutate_only_the_selected_feature() {
        use crate::db::AiFeature;
        let state = test_state().await;
        let app = router(state.clone(), Some("test-key".into()));
        let path = "/api/admin/chats/-31/features/prompt";
        let body = r#"{"enabled":true}"#;
        for (method, path) in [
            ("PUT", path),
            ("GET", "/api/admin/chats"),
            ("GET", "/api/admin/sessions"),
            ("GET", "/api/admin/sessions/-31/2"),
        ] {
            for token in [None, Some("wrong")] {
                let res = admin_request(&app, method, path, token, body).await;
                assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
                assert_eq!(res.headers()[header::CACHE_CONTROL], "no-store");
            }
        }
        assert_eq!(
            admin_request(
                &router(state.clone(), None),
                "PUT",
                path,
                Some("test-key"),
                body
            )
            .await
            .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert!(
            !state
                .db
                .feature_enabled(-31, AiFeature::Prompt)
                .await
                .unwrap()
        );
        let token = Some("test-key");
        for path in [
            "/api/admin/chats/0/features/prompt",
            "/api/admin/chats/4503599627370496/features/prompt",
            "/api/admin/chats/-31/features/unknown",
        ] {
            assert_eq!(
                admin_request(&app, "PUT", path, token, body).await.status(),
                StatusCode::BAD_REQUEST
            );
        }
        assert_eq!(
            admin_request(&app, "PUT", path, token, body).await.status(),
            StatusCode::NO_CONTENT
        );
        assert!(
            state
                .db
                .feature_enabled(-31, AiFeature::Prompt)
                .await
                .unwrap()
        );
        assert!(
            !state
                .db
                .feature_enabled(-32, AiFeature::Prompt)
                .await
                .unwrap()
        );
        assert!(
            !state
                .db
                .feature_enabled(-31, AiFeature::Diff)
                .await
                .unwrap()
        );
        let public_chats = admin_request(&app, "GET", "/api/chats", None, "").await;
        assert_eq!(
            to_bytes(public_chats.into_body(), 100)
                .await
                .unwrap()
                .as_ref(),
            b"[]"
        );
    }

    #[tokio::test]
    async fn session_browser_groups_branches_and_scopes_transcripts_to_chat() {
        let state = test_state().await;
        for (chat, id, parent, question) in [
            (-1, 1, None, "root"),
            (-1, 2, Some(1), "left"),
            (-1, 3, Some(1), "right"),
            (-2, 1, None, "private"),
        ] {
            state
                .db
                .save_prompt_turn(
                    chat,
                    Some(7),
                    id,
                    parent,
                    &crate::db::DeliveredTurn {
                        question,
                        answer: "answer",
                        author: Some(987654321),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }
        let app = router(state, Some("test-key".into()));
        let json = async |path: &str| {
            let res = admin_request(&app, "GET", path, Some("test-key"), "").await;
            assert_eq!(res.status(), StatusCode::OK);
            let body = to_bytes(res.into_body(), 8192).await.unwrap();
            let text = std::str::from_utf8(&body).unwrap();
            assert!(!text.contains("987654321") && !text.contains("private"));
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()
        };
        let list = json("/api/admin/sessions?chat=-1").await;
        assert_eq!(list["items"].as_array().unwrap().len(), 1);
        assert_eq!(list["items"][0]["turns"], 3);
        assert_eq!(list["items"][0]["topic"], 7);
        let turns = json("/api/admin/sessions/-1/1").await;
        assert_eq!(turns[2]["question"], "right");
        assert_eq!(turns[2]["parent"], 1);
        assert_eq!(turns[1]["parent"], 1);
        assert_eq!(
            admin_request(
                &app,
                "GET",
                "/api/admin/sessions/-3/1",
                Some("test-key"),
                ""
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
    }
}
