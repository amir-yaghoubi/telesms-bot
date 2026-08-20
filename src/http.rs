use std::sync::Arc;

use axum::body::Body;
use axum::extract::{FromRequest, Path, Query, Request, State};
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use subtle::ConstantTimeEq;
use tokio_util::sync::CancellationToken;

use crate::actions::{
    self, ActionError, Identity, NumberState, Opened, SmsSent, Who,
};
use crate::app::TelegramSink;
use crate::config::Config;
use crate::db::Db;
use crate::modem::{ModemInfo, SmsModem};
use crate::route::GENERAL_THREAD;

#[derive(Clone)]
pub struct HttpState {
    pub cfg: Config,
    pub db: Arc<Db>,
    pub modem: Arc<dyn SmsModem>,
    pub info: Arc<dyn ModemInfo>,
    pub tg: Arc<dyn TelegramSink>,
}

pub fn router(state: HttpState) -> Router {
    let api = Router::new()
        .route("/status", get(status_handler))
        .route("/search", post(search_handler))
        .route("/sms", post(sms_handler))
        .route("/open", post(open_handler))
        .route("/who", post(who_handler))
        .route("/number", post(number_handler))
        .route("/ignore", post(ignore_handler))
        .route("/chats", get(chats_handler))
        .route("/chats/{thread_id}/messages", get(chat_messages_handler))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        .method_not_allowed_fallback(api_method_not_allowed)
        .fallback(api_not_found);

    Router::new()
        .route("/health", get(health))
        .nest("/api/v1", api)
        .fallback(not_found)
        .with_state(state)
}

pub async fn serve(
    state: HttpState,
    bind: std::net::SocketAddr,
    cancel: CancellationToken,
) {
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .expect("api bind");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            cancel.cancelled().await;
        })
        .await
        .ok();
}

pub fn action_to_response(err: ActionError) -> (StatusCode, Json<Value>) {
    match err {
        ActionError::MissingIdentity => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "missing_identity",
                "message": "missing identity",
            })),
        ),
        ActionError::Validation(msg) => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "validation",
                "message": msg,
            })),
        ),
        ActionError::InvalidNumber(msg) => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "invalid_number",
                "message": msg,
            })),
        ),
        ActionError::NotFound(msg) => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "not_found",
                "message": msg,
            })),
        ),
        ActionError::IdentityConflict => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "identity_conflict",
                "message": "identity fields disagree",
            })),
        ),
        ActionError::NeedDefaultNumber { numbers } => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "need_default_number",
                "message": "need default number",
                "numbers": numbers,
            })),
        ),
        ActionError::ContactsUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "contacts_unavailable",
                "message": "contacts unavailable",
            })),
        ),
        ActionError::ModemFailed(msg) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": "modem_failed",
                "message": msg,
            })),
        ),
        ActionError::TelegramFailed { sent, message } => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": "telegram_failed",
                "message": message,
                "sent": sent,
            })),
        ),
        ActionError::Db(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": "internal",
                "message": e.to_string(),
            })),
        ),
        ActionError::App(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": "internal",
                "message": e.to_string(),
            })),
        ),
    }
}

#[derive(Debug, Deserialize)]
struct IdentityReq {
    #[serde(default)]
    number: Option<String>,
    #[serde(default)]
    contact_id: Option<i64>,
    #[serde(default)]
    thread_id: Option<i32>,
}

fn identity_from_req(req: IdentityReq) -> Result<Identity, ActionError> {
    let number = req
        .number
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let has_number = number.is_some();
    let has_contact = req.contact_id.is_some();
    let has_thread = req.thread_id.is_some();

    if has_thread && !has_number && !has_contact {
        return Err(ActionError::MissingIdentity);
    }

    Ok(Identity {
        number,
        contact_id: req.contact_id,
        thread_id: req.thread_id,
    })
}

fn api_key_matches(expected: &str, provided: &str) -> bool {
    let max_len = expected.len().max(provided.len());
    let mut expected_buf = vec![0u8; max_len];
    let mut provided_buf = vec![0u8; max_len];
    if max_len > 0 {
        expected_buf[..expected.len()].copy_from_slice(expected.as_bytes());
        provided_buf[..provided.len()].copy_from_slice(provided.as_bytes());
    }
    let contents_equal = bool::from(expected_buf.as_slice().ct_eq(provided_buf.as_slice()));
    contents_equal && expected.len() == provided.len()
}

async fn auth_middleware(
    State(state): State<HttpState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(expected) = state.cfg.api_key.as_ref().filter(|k| !k.is_empty()) else {
        return unauthorized();
    };
    let Some(header) = request
        .headers()
        .get("X-Api-Key")
        .and_then(|v| v.to_str().ok())
    else {
        return unauthorized();
    };
    if !api_key_matches(expected, header) {
        return unauthorized();
    }
    next.run(request).await
}

fn api_error_response(status: StatusCode, error: &str, message: &str) -> Response {
    (
        status,
        Json(json!({
            "error": error,
            "message": message,
        })),
    )
        .into_response()
}

fn json_rejection_response(rejection: JsonRejection) -> Response {
    let message = match rejection {
        JsonRejection::JsonDataError(err) => err.to_string(),
        JsonRejection::JsonSyntaxError(err) => err.to_string(),
        JsonRejection::MissingJsonContentType(_) => {
            "expected request with content-type application/json".into()
        }
        JsonRejection::BytesRejection(err) => err.to_string(),
        _ => "invalid request body".into(),
    };
    api_error_response(StatusCode::BAD_REQUEST, "validation", &message)
}

struct ApiJson<T>(T);

impl<T> std::ops::Deref for ApiJson<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T, S> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(ApiJson(value)),
            Err(rejection) => Err(json_rejection_response(rejection)),
        }
    }
}

async fn api_method_not_allowed() -> Response {
    api_error_response(
        StatusCode::METHOD_NOT_ALLOWED,
        "validation",
        "method not allowed",
    )
}

async fn api_not_found() -> Response {
    api_error_response(StatusCode::NOT_FOUND, "not_found", "not found")
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": "unauthorized",
            "message": "unauthorized",
        })),
    )
        .into_response()
}

async fn health() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}

async fn not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": "not_found",
            "message": "not found",
        })),
    )
}

async fn status_handler(State(state): State<HttpState>) -> Response {
    match actions::status(
        state.info.as_ref(),
        state.db.as_ref(),
        state.cfg.status_tz,
        &state.cfg.modem_uid,
    )
    .await
    {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => action_to_response(e).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct SearchReq {
    query: String,
}

async fn search_handler(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<SearchReq>,
) -> Response {
    match actions::search_contacts(state.db.as_ref(), &body.query) {
        Ok(contacts) => {
            let contacts: Vec<Value> = contacts
                .iter()
                .map(|c| {
                    json!({
                        "id": c.id,
                        "display_name": c.display_name,
                        "numbers": c.numbers,
                        "ambiguous": c.ambiguous,
                    })
                })
                .collect();
            (StatusCode::OK, Json(json!({ "contacts": contacts }))).into_response()
        }
        Err(e) => action_to_response(e).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct SmsReq {
    #[serde(default)]
    number: Option<String>,
    #[serde(default)]
    contact_id: Option<i64>,
    #[serde(default)]
    thread_id: Option<i32>,
    text: String,
}

async fn sms_handler(State(state): State<HttpState>, ApiJson(body): ApiJson<SmsReq>) -> Response {
    let id = match identity_from_req(IdentityReq {
        number: body.number,
        contact_id: body.contact_id,
        thread_id: body.thread_id,
    }) {
        Ok(id) => id,
        Err(e) => return action_to_response(e).into_response(),
    };
    let reply_thread = id.thread_id.unwrap_or(GENERAL_THREAD);
    match actions::send_sms(
        state.db.as_ref(),
        &state.cfg.default_region,
        &id,
        &body.text,
        reply_thread,
        None,
        state.modem.as_ref(),
        state.tg.as_ref(),
        state.cfg.sms_delete_enabled,
    )
    .await
    {
        Ok(s) => (StatusCode::OK, Json(sms_sent_json(&s))).into_response(),
        Err(e) => action_to_response(e).into_response(),
    }
}

async fn open_handler(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<IdentityReq>,
) -> Response {
    let id = match identity_from_req(body) {
        Ok(id) => id,
        Err(e) => return action_to_response(e).into_response(),
    };
    match actions::open_topic(
        state.db.as_ref(),
        &state.cfg.default_region,
        &id,
        state.tg.as_ref(),
    )
    .await
    {
        Ok(o) => (StatusCode::OK, Json(opened_json(&o))).into_response(),
        Err(e) => action_to_response(e).into_response(),
    }
}

async fn who_handler(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<IdentityReq>,
) -> Response {
    let id = match identity_from_req(body) {
        Ok(id) => id,
        Err(e) => return action_to_response(e).into_response(),
    };
    match actions::who(state.db.as_ref(), &state.cfg.default_region, &id) {
        Ok(w) => (StatusCode::OK, Json(who_json(&w))).into_response(),
        Err(e) => action_to_response(e).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct NumberReq {
    #[serde(default)]
    number: Option<String>,
    #[serde(default)]
    contact_id: Option<i64>,
    #[serde(default)]
    thread_id: Option<i32>,
    default: Option<String>,
}

async fn number_handler(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<NumberReq>,
) -> Response {
    let id = match identity_from_req(IdentityReq {
        number: body.number,
        contact_id: body.contact_id,
        thread_id: body.thread_id,
    }) {
        Ok(id) => id,
        Err(e) => return action_to_response(e).into_response(),
    };
    let result = match body.default {
        None => actions::list_numbers(state.db.as_ref(), &state.cfg.default_region, &id),
        Some(ref new_default) => {
            actions::set_default_number(
                state.db.as_ref(),
                &state.cfg.default_region,
                &id,
                new_default,
                state.tg.as_ref(),
            )
            .await
        }
    };
    match result {
        Ok(st) => (StatusCode::OK, Json(number_state_json(&st))).into_response(),
        Err(e) => action_to_response(e).into_response(),
    }
}

async fn ignore_handler(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<IdentityReq>,
) -> Response {
    let id = match identity_from_req(body) {
        Ok(id) => id,
        Err(e) => return action_to_response(e).into_response(),
    };
    match actions::ignore(
        state.db.as_ref(),
        &state.cfg.default_region,
        &id,
        state.tg.as_ref(),
    )
    .await
    {
        Ok(numbers) => (StatusCode::OK, Json(json!({ "ignored": numbers }))).into_response(),
        Err(e) => action_to_response(e).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct HistoryQuery {
    limit: Option<i64>,
    before: Option<String>,
    after: Option<String>,
    number: Option<String>,
    contact_id: Option<i64>,
}

async fn chats_handler(
    State(state): State<HttpState>,
    Query(q): Query<HistoryQuery>,
) -> Response {
    match actions::list_chats(
        state.db.as_ref(),
        q.limit,
        q.before.as_deref(),
        q.after.as_deref(),
    ) {
        Ok(body) => (StatusCode::OK, Json(body)).into_response(),
        Err(err) => action_to_response(err).into_response(),
    }
}

async fn chat_messages_handler(
    State(state): State<HttpState>,
    Path(thread_id): Path<i32>,
    Query(q): Query<HistoryQuery>,
) -> Response {
    match actions::list_messages(
        state.db.as_ref(),
        &state.cfg.default_region,
        thread_id,
        q.limit,
        q.before.as_deref(),
        q.after.as_deref(),
        q.number.as_deref(),
        q.contact_id,
    ) {
        Ok(body) => (StatusCode::OK, Json(body)).into_response(),
        Err(err) => action_to_response(err).into_response(),
    }
}

fn sms_sent_json(s: &SmsSent) -> Value {
    json!({
        "e164": s.e164,
        "thread_id": s.thread_id,
        "sent": s.sent,
    })
}

fn opened_json(o: &Opened) -> Value {
    json!({
        "contact_id": o.contact_id,
        "thread_id": o.thread_id,
        "title": o.title,
        "created": o.created,
    })
}

fn who_json(w: &Who) -> Value {
    json!({
        "thread_id": w.thread_id,
        "contact_id": w.contact_id,
        "display_name": w.display_name,
        "numbers": w.numbers,
        "default_e164": w.default_e164,
        "ambiguous": w.ambiguous,
    })
}

fn number_state_json(st: &NumberState) -> Value {
    json!({
        "thread_id": st.thread_id,
        "numbers": st.numbers,
        "default_e164": st.default_e164,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::FakeTg;
    use crate::db::Topic;
    use crate::modem::FakeModem;
    use http_body_util::BodyExt;
    use std::path::PathBuf;
    use std::time::Duration;
    use tower::ServiceExt;

    async fn call(app: Router, req: axum::http::Request<Body>) -> axum::http::Response<Body> {
        app.oneshot(req).await.unwrap()
    }

    async fn body_json(res: axum::http::Response<Body>) -> Value {
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn test_config(api_key: &str) -> Config {
        Config {
            telegram_bot_token: "tok".into(),
            telegram_user_id: 42,
            telegram_group_id: -1001,
            modem_uid: "dwm222".into(),
            google_client_id: "cid".into(),
            google_client_secret: "sec".into(),
            google_token_path: PathBuf::from("./secrets/google-token.json"),
            database_path: PathBuf::from("./data/telesms.sqlite"),
            contacts_sync_interval: Duration::from_secs(21600),
            default_region: "IR".into(),
            status_tz: chrono_tz::UTC,
            sms_delete_enabled: true,
            sms_delete_max_age: Duration::from_secs(30 * 86400),
            api_key: Some(api_key.into()),
            api_bind: "127.0.0.1".into(),
            api_port: 8787,
        }
    }

    fn test_router(api_key: &str) -> Router {
        let db = Arc::new({
            let db = Db::open_in_memory().unwrap();
            let id = db.upsert_contact("people/a", "Ali").unwrap();
            db.replace_contact_numbers(id, &["+989121234567".into()])
                .unwrap();
            db.upsert_topic(&Topic {
                thread_id: 9,
                contact_id: Some(id),
                default_e164: Some("+989121234567".into()),
                title: "Ali".into(),
                ignored: false,
            })
            .unwrap();
            db
        });
        let modem = Arc::new(FakeModem::default());
        let info = modem.clone() as Arc<dyn ModemInfo>;
        let modem = modem as Arc<dyn SmsModem>;
        let tg = Arc::new(FakeTg::new()) as Arc<dyn TelegramSink>;
        router(HttpState {
            cfg: test_config(api_key),
            db,
            modem,
            info,
            tg,
        })
    }

    #[tokio::test]
    async fn health_no_key() {
        let app = test_router("k");
        let res = call(
            app,
            axum::http::Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn status_requires_key() {
        let app = test_router("k");
        let res = call(
            app,
            axum::http::Request::builder()
                .uri("/api/v1/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn status_with_key_ok() {
        let app = test_router("k");
        let res = call(
            app,
            axum::http::Request::builder()
                .uri("/api/v1/status")
                .header("X-Api-Key", "k")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn search_and_sms_roundtrip() {
        let app = test_router("k");
        let res = call(
            app,
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/v1/sms")
                .header("X-Api-Key", "k")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"number":"09121234567","text":"hi"}"#,
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[test]
    fn api_key_length_mismatch_rejected() {
        assert!(!api_key_matches("secret", "sec"));
        assert!(!api_key_matches("secret", "secretx"));
        assert!(api_key_matches("secret", "secret"));
    }

    #[tokio::test]
    async fn invalid_json_returns_envelope() {
        let app = test_router("k");
        let res = call(
            app,
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/v1/who")
                .header("X-Api-Key", "k")
                .header("content-type", "application/json")
                .body(Body::from("{not json"))
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let body = body_json(res).await;
        assert_eq!(body["error"], "validation");
        assert!(body["message"].is_string());
    }

    #[tokio::test]
    async fn wrong_method_returns_envelope() {
        let app = test_router("k");
        let res = call(
            app,
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/v1/status")
                .header("X-Api-Key", "k")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
        let body = body_json(res).await;
        assert_eq!(body["error"], "validation");
        assert_eq!(body["message"], "method not allowed");
    }

    #[tokio::test]
    async fn unknown_api_route_returns_envelope() {
        let app = test_router("k");
        let res = call(
            app,
            axum::http::Request::builder()
                .uri("/api/v1/missing")
                .header("X-Api-Key", "k")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let body = body_json(res).await;
        assert_eq!(body["error"], "not_found");
        assert_eq!(body["message"], "not found");
    }

    #[tokio::test]
    async fn wrong_key_length_unauthorized() {
        let app = test_router("secret");
        let res = call(
            app,
            axum::http::Request::builder()
                .uri("/api/v1/status")
                .header("X-Api-Key", "sec")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let body = body_json(res).await;
        assert_eq!(body["error"], "unauthorized");
    }

    #[tokio::test]
    async fn thread_id_only_rejected() {
        let app = test_router("k");
        let res = call(
            app,
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/v1/who")
                .header("X-Api-Key", "k")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"thread_id":9}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn chats_requires_api_key() {
        let app = test_router("secret");
        let res = call(
            app,
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/v1/chats")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn chats_empty_ok() {
        let app = test_router("secret");
        let res = call(
            app,
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/v1/chats")
                .header("X-Api-Key", "secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["chats"], json!([]));
    }

    #[tokio::test]
    async fn messages_unknown_thread_404() {
        let app = test_router("secret");
        let res = call(
            app,
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/v1/chats/99/messages")
                .header("X-Api-Key", "secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let v = body_json(res).await;
        assert_eq!(v["error"], "not_found");
    }

    fn test_router_with_inbound(api_key: &str) -> Router {
        let db = Arc::new({
            let db = Db::open_in_memory().unwrap();
            let id = db.upsert_contact("people/a", "Ali").unwrap();
            db.replace_contact_numbers(id, &["+989121234567".into()])
                .unwrap();
            db.upsert_topic(&Topic {
                thread_id: 9,
                contact_id: Some(id),
                default_e164: Some("+989121234567".into()),
                title: "Ali".into(),
                ignored: false,
            })
            .unwrap();
            db.record_inbound("/g", "+989121234567", "hello", None, "", 9)
                .unwrap();
            db
        });
        let modem = Arc::new(FakeModem::default());
        let info = modem.clone() as Arc<dyn ModemInfo>;
        let modem = modem as Arc<dyn SmsModem>;
        let tg = Arc::new(FakeTg::new()) as Arc<dyn TelegramSink>;
        router(HttpState {
            cfg: test_config(api_key),
            db,
            modem,
            info,
            tg,
        })
    }

    #[tokio::test]
    async fn messages_known_thread_ok() {
        let app = test_router_with_inbound("secret");
        let res = call(
            app,
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/v1/chats/9/messages")
                .header("X-Api-Key", "secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["thread_id"], 9);
        assert_eq!(v["messages"].as_array().unwrap().len(), 1);
        assert_eq!(v["messages"][0]["body"], "hello");
    }
}
