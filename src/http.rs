use axum::body::Body;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use subtle::ConstantTimeEq;
use tokio_util::sync::CancellationToken;

use crate::actions::{self, ActionError, Identity, NumberState, Opened, SmsSent, Who};
use crate::app::AppState;
use crate::route::GENERAL_THREAD;
use crate::sorting::learn::learn_otp;

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/status", get(status_handler))
        .route("/search", post(search_handler))
        .route("/sms", post(sms_handler))
        .route("/open", post(open_handler))
        .route("/who", post(who_handler))
        .route("/number", post(number_handler))
        .route("/ignore", post(ignore_handler))
        .route("/call-forward", get(call_forward_get).put(call_forward_put))
        .route("/chats", get(chats_handler))
        .route("/chats/{thread_id}/messages", get(chat_messages_handler))
        .route(
            "/otp-patterns",
            get(otp_patterns_list).post(otp_patterns_add),
        )
        .route("/otp-patterns/learn", post(otp_patterns_learn))
        .route("/otp-patterns/{id}", delete(otp_patterns_delete))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .method_not_allowed_fallback(api_method_not_allowed)
        .fallback(api_not_found);

    Router::new()
        .route("/health", get(health))
        .nest("/api/v1", api)
        .fallback(not_found)
        .with_state(state)
}

pub async fn serve(state: AppState, bind: std::net::SocketAddr, cancel: CancellationToken) {
    let listener = tokio::net::TcpListener::bind(bind).await.expect("api bind");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            cancel.cancelled().await;
        })
        .await
        .ok();
}

pub fn action_to_response(err: ActionError) -> (StatusCode, Json<Value>) {
    let status = match &err {
        ActionError::MissingIdentity
        | ActionError::Validation(_)
        | ActionError::InvalidNumber(_) => StatusCode::BAD_REQUEST,
        ActionError::NotFound(_) => StatusCode::NOT_FOUND,
        ActionError::IdentityConflict | ActionError::NeedDefaultNumber { .. } => {
            StatusCode::CONFLICT
        }
        ActionError::ContactsUnavailable | ActionError::ModemUnavailable(_) => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        ActionError::ModemFailed(_) | ActionError::TelegramFailed { .. } => StatusCode::BAD_GATEWAY,
        ActionError::ForwardFailed(_) | ActionError::Db(_) | ActionError::App(_) => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    (status, Json(err.error_body()))
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
    actions::identity_from_parts(req.number, req.contact_id, req.thread_id)
}

pub(crate) fn api_key_matches(expected: &str, provided: &str) -> bool {
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

async fn auth_middleware(State(state): State<AppState>, request: Request, next: Next) -> Response {
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

async fn status_handler(State(state): State<AppState>) -> Response {
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

async fn call_forward_get(State(state): State<AppState>) -> Response {
    match actions::get_call_forward(state.forward.as_ref(), &state.cfg.default_region).await {
        Ok(st) => Json(json!({
            "enabled": st.enabled,
            "e164": st.e164,
        }))
        .into_response(),
        Err(err) => action_to_response(err).into_response(),
    }
}

async fn call_forward_put(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<Value>,
) -> Response {
    let Some(value) = body.get("e164") else {
        return action_to_response(ActionError::Validation("e164 is required".into()))
            .into_response();
    };
    let e164 = match value {
        Value::Null => None,
        Value::String(value) if value.trim().is_empty() => {
            return action_to_response(ActionError::Validation("e164 must not be empty".into()))
                .into_response();
        }
        Value::String(value) => Some(value.clone()),
        _ => {
            return action_to_response(ActionError::Validation(
                "e164 must be string or null".into(),
            ))
            .into_response();
        }
    };

    match actions::put_call_forward(state.forward.as_ref(), &state.cfg.default_region, e164).await {
        Ok(st) => Json(json!({
            "enabled": st.enabled,
            "e164": st.e164,
        }))
        .into_response(),
        Err(err) => action_to_response(err).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct SearchReq {
    query: String,
}

async fn search_handler(
    State(state): State<AppState>,
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

async fn sms_handler(State(state): State<AppState>, ApiJson(body): ApiJson<SmsReq>) -> Response {
    let id = match identity_from_req(IdentityReq {
        number: body.number,
        contact_id: body.contact_id,
        thread_id: body.thread_id,
    }) {
        Ok(id) => id,
        Err(e) => return action_to_response(e).into_response(),
    };
    let reply_thread = id.thread_id.unwrap_or(GENERAL_THREAD);
    let ctx = crate::app::SendContext {
        modem: state.modem.as_ref(),
        tg: state.tg.as_ref(),
        bus: &state.bus,
        delete_enabled: state.cfg.sms_delete_enabled,
    };
    match actions::send_sms(
        state.db.as_ref(),
        &state.cfg.default_region,
        &id,
        &body.text,
        reply_thread,
        None,
        &ctx,
    )
    .await
    {
        Ok(s) => (StatusCode::OK, Json(sms_sent_json(&s))).into_response(),
        Err(e) => action_to_response(e).into_response(),
    }
}

async fn open_handler(
    State(state): State<AppState>,
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
    State(state): State<AppState>,
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
    State(state): State<AppState>,
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
    State(state): State<AppState>,
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
    category: Option<String>,
    tag: Option<String>,
}

async fn chats_handler(
    State(state): State<AppState>,
    query: Result<Query<HistoryQuery>, QueryRejection>,
) -> Response {
    let Query(q) = match query {
        Ok(query) => query,
        Err(rejection) => {
            return api_error_response(
                StatusCode::BAD_REQUEST,
                "validation",
                &rejection.to_string(),
            );
        }
    };
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
    State(state): State<AppState>,
    path: Result<Path<i32>, PathRejection>,
    query: Result<Query<HistoryQuery>, QueryRejection>,
) -> Response {
    let Path(thread_id) = match path {
        Ok(path) => path,
        Err(rejection) => {
            return api_error_response(
                StatusCode::BAD_REQUEST,
                "validation",
                &rejection.to_string(),
            );
        }
    };
    let Query(q) = match query {
        Ok(query) => query,
        Err(rejection) => {
            return api_error_response(
                StatusCode::BAD_REQUEST,
                "validation",
                &rejection.to_string(),
            );
        }
    };
    let filter = match actions::message_filter(q.category.as_deref(), q.tag.as_deref()) {
        Ok(filter) => filter,
        Err(err) => return action_to_response(err).into_response(),
    };
    match actions::list_messages(
        state.db.as_ref(),
        &state.cfg.default_region,
        thread_id,
        q.limit,
        q.before.as_deref(),
        q.after.as_deref(),
        q.number.as_deref(),
        q.contact_id,
        &filter,
    ) {
        Ok(body) => (StatusCode::OK, Json(body)).into_response(),
        Err(err) => action_to_response(err).into_response(),
    }
}

async fn otp_patterns_list(State(state): State<AppState>) -> Response {
    match state.db.otp_patterns() {
        Ok(patterns) => Json(json!({ "patterns": patterns })).into_response(),
        Err(err) => action_to_response(err.into()).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct PatternReq {
    #[serde(default)]
    name: Option<String>,
    pattern: String,
}

async fn otp_patterns_add(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<PatternReq>,
) -> Response {
    match actions::add_otp_pattern(state.db.as_ref(), body.name.as_deref(), &body.pattern) {
        Ok((row, created)) => {
            let status = if created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            (status, Json(row)).into_response()
        }
        Err(err) => action_to_response(err).into_response(),
    }
}

async fn otp_patterns_delete(
    State(state): State<AppState>,
    path: Result<Path<i64>, PathRejection>,
) -> Response {
    let Path(id) = match path {
        Ok(path) => path,
        Err(rejection) => {
            return api_error_response(
                StatusCode::BAD_REQUEST,
                "validation",
                &rejection.to_string(),
            );
        }
    };
    match actions::delete_otp_pattern(state.db.as_ref(), id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => action_to_response(err).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct LearnReq {
    #[serde(default)]
    message_id: Option<String>,
    #[serde(default)]
    body: Option<String>,
    code: String,
    #[serde(default)]
    name: Option<String>,
}

async fn otp_patterns_learn(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<LearnReq>,
) -> Response {
    let source = match actions::learn_source(body.message_id.as_deref(), body.body.as_deref()) {
        Ok(source) => source,
        Err(err) => return action_to_response(err).into_response(),
    };
    let name = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty());
    match learn_otp(
        state.db.as_ref(),
        state.tg.as_ref(),
        &state.bus,
        &state.sorting.config,
        source,
        &body.code,
        name,
    )
    .await
    {
        Ok(learned) => Json(json!({
            "pattern": learned.pattern,
            "created": learned.created,
            "message": learned.message.map(actions::MessageItem::from),
        }))
        .into_response(),
        Err(err) => action_to_response(err.into()).into_response(),
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
    use crate::app::{FakeTg, TelegramSink};
    use crate::config::Config;
    use crate::db::{Db, Topic};
    use crate::modem::{CallForward, FakeModem, ModemInfo, SmsModem};
    use http_body_util::BodyExt;
    use std::path::PathBuf;
    use std::sync::Arc;
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
            ws_enabled: false,
            ws_bind: "127.0.0.1".into(),
            ws_port: 8788,
            classifier_url: None,
            classifier_model: None,
            sorting_config_path: None,
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
                sort: None,
            })
            .unwrap();
            db
        });
        let modem = Arc::new(FakeModem::default());
        let info = modem.clone() as Arc<dyn ModemInfo>;
        let forward = modem.clone() as Arc<dyn CallForward>;
        let modem = modem as Arc<dyn SmsModem>;
        let tg = Arc::new(FakeTg::new()) as Arc<dyn TelegramSink>;
        router(AppState {
            cfg: test_config(api_key),
            db,
            modem,
            info,
            forward,
            tg,
            bus: crate::events::EventBus::new(crate::events::EVENT_BUFFER),
            sorting: crate::sorting::Sorting::disabled(),
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
        let body = body_json(res).await;
        assert!(body.get("call_forward").is_none());
        assert!(body.get("forward").is_none());
    }

    #[tokio::test]
    async fn call_forward_get_put_and_disable() {
        let app = test_router("k");

        let res = call(
            app.clone(),
            axum::http::Request::builder()
                .uri("/api/v1/call-forward")
                .header("X-Api-Key", "k")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            body_json(res).await,
            json!({"enabled": false, "e164": null})
        );

        let res = call(
            app.clone(),
            axum::http::Request::builder()
                .method("PUT")
                .uri("/api/v1/call-forward")
                .header("X-Api-Key", "k")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"e164":"09121234567"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            body_json(res).await,
            json!({"enabled": true, "e164": "+989121234567"})
        );

        let res = call(
            app.clone(),
            axum::http::Request::builder()
                .uri("/api/v1/call-forward")
                .header("X-Api-Key", "k")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            body_json(res).await,
            json!({"enabled": true, "e164": "+989121234567"})
        );

        let res = call(
            app,
            axum::http::Request::builder()
                .method("PUT")
                .uri("/api/v1/call-forward")
                .header("X-Api-Key", "k")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"e164":null}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            body_json(res).await,
            json!({"enabled": false, "e164": null})
        );
    }

    #[tokio::test]
    async fn call_forward_put_requires_nonempty_e164() {
        for body in [r#"{}"#, r#"{"e164":""}"#, r#"{"e164":"  "}"#] {
            let res = call(
                test_router("k"),
                axum::http::Request::builder()
                    .method("PUT")
                    .uri("/api/v1/call-forward")
                    .header("X-Api-Key", "k")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await;
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "body: {body}");
            assert_eq!(body_json(res).await["error"], "validation");
        }
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
                .body(Body::from(r#"{"number":"09121234567","text":"hi"}"#))
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
    async fn invalid_history_limit_returns_validation_envelope() {
        let app = test_router("secret");
        let res = call(
            app,
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/v1/chats?limit=abc")
                .header("X-Api-Key", "secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let v = body_json(res).await;
        assert_eq!(v["error"], "validation");
        assert!(v["message"].is_string());
    }

    #[tokio::test]
    async fn invalid_history_path_returns_validation_envelope() {
        let app = test_router("secret");
        let res = call(
            app,
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/v1/chats/abc/messages")
                .header("X-Api-Key", "secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let v = body_json(res).await;
        assert_eq!(v["error"], "validation");
        assert!(v["message"].is_string());
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
                sort: None,
            })
            .unwrap();
            db.record_inbound("/g", "+989121234567", "hello", None, "", 9, None, None)
                .unwrap();
            db
        });
        let modem = Arc::new(FakeModem::default());
        let info = modem.clone() as Arc<dyn ModemInfo>;
        let forward = modem.clone() as Arc<dyn CallForward>;
        let modem = modem as Arc<dyn SmsModem>;
        let tg = Arc::new(FakeTg::new()) as Arc<dyn TelegramSink>;
        router(AppState {
            cfg: test_config(api_key),
            db,
            modem,
            info,
            forward,
            tg,
            bus: crate::events::EventBus::new(crate::events::EVENT_BUFFER),
            sorting: crate::sorting::Sorting::disabled(),
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

    fn sorting_db() -> Arc<Db> {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989121234567".into()])
            .unwrap();
        for (thread_id, contact_id, title) in [(9, Some(id), "Ali"), (15, None, "+989129990000")] {
            db.upsert_topic(&Topic {
                thread_id,
                contact_id,
                default_e164: None,
                title: title.into(),
                ignored: false,
                sort: None,
            })
            .unwrap();
        }
        db.upsert_sorted_topic("promotional", 12, "Promotions")
            .unwrap();
        Arc::new(db)
    }

    fn stored(category: &str, confidence: f64, tags: &[&str]) -> crate::db::StoredClassification {
        crate::db::StoredClassification {
            public: crate::db::Classification {
                category: category.into(),
                confidence,
                tags: tags.iter().map(|t| t.to_string()).collect(),
            },
            scores: json!({}),
        }
    }

    fn router_for(api_key: &str, db: Arc<Db>) -> Router {
        let modem = Arc::new(FakeModem::default());
        let info = modem.clone() as Arc<dyn ModemInfo>;
        let forward = modem.clone() as Arc<dyn CallForward>;
        let modem = modem as Arc<dyn SmsModem>;
        let tg = Arc::new(FakeTg::new()) as Arc<dyn TelegramSink>;
        router(AppState {
            cfg: test_config(api_key),
            db,
            modem,
            info,
            forward,
            tg,
            bus: crate::events::EventBus::new(crate::events::EVENT_BUFFER),
            sorting: crate::sorting::Sorting::disabled(),
        })
    }

    fn get(uri: &str) -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .uri(uri)
            .header("X-Api-Key", "k")
            .body(Body::empty())
            .unwrap()
    }

    fn send_json(method: &str, uri: &str, body: &str) -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("X-Api-Key", "k")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn delete(uri: &str) -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .method("DELETE")
            .uri(uri)
            .header("X-Api-Key", "k")
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn messages_include_classification_and_otp_code() {
        let db = sorting_db();
        let c = stored("transactional", 0.9, &["scam"]);
        db.record_inbound(
            "/1",
            "+989121234567",
            "code 123456",
            None,
            "",
            9,
            Some("123456"),
            Some(&c),
        )
        .unwrap();
        db.record_inbound("/2", "+989121234567", "plain", None, "", 9, None, None)
            .unwrap();
        let res = call(router_for("k", db), get("/api/v1/chats/9/messages")).await;
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        let messages = v["messages"].as_array().unwrap();
        let with = messages
            .iter()
            .find(|m| m["body"] == "code 123456")
            .unwrap();
        assert_eq!(
            with["classification"],
            json!({"category": "transactional", "confidence": 0.9, "tags": ["scam"]})
        );
        assert_eq!(with["otp_code"], "123456");
        let without = messages.iter().find(|m| m["body"] == "plain").unwrap();
        assert_eq!(without["classification"], Value::Null);
        assert_eq!(without["otp_code"], Value::Null);
    }

    #[tokio::test]
    async fn messages_filter_by_category_and_tag() {
        let db = sorting_db();
        for (path, body, c) in [
            ("/1", "promo", stored("promotional", 0.8, &[])),
            ("/2", "scam promo", stored("promotional", 0.7, &["scam"])),
            ("/3", "bank", stored("transactional", 0.9, &[])),
        ] {
            db.record_inbound(path, "+989121234567", body, None, "", 9, None, Some(&c))
                .unwrap();
        }
        let app = router_for("k", db);

        let res = call(
            app.clone(),
            get("/api/v1/chats/9/messages?category=promotional"),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            body_json(res).await["messages"].as_array().unwrap().len(),
            2
        );

        let res = call(app.clone(), get("/api/v1/chats/9/messages?tag=scam")).await;
        let v = body_json(res).await;
        assert_eq!(v["messages"].as_array().unwrap().len(), 1);
        assert_eq!(v["messages"][0]["body"], "scam promo");

        let res = call(
            app,
            get("/api/v1/chats/9/messages?category=transactional&tag=scam"),
        )
        .await;
        assert_eq!(body_json(res).await["messages"], json!([]));
    }

    #[tokio::test]
    async fn unknown_category_is_validation_error() {
        let app = router_for("k", sorting_db());
        for uri in [
            "/api/v1/chats/9/messages?category=bogus",
            "/api/v1/chats/9/messages?tag=bogus",
        ] {
            let res = call(app.clone(), get(uri)).await;
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{uri}");
            let v = body_json(res).await;
            assert_eq!(v["error"], "validation", "{uri}");
            assert!(v["message"].is_string());
        }
    }

    #[tokio::test]
    async fn chats_report_kind_and_sort() {
        let db = sorting_db();
        for (path, thread_id) in [("/1", 1), ("/2", 9), ("/3", 12), ("/4", 15)] {
            db.record_inbound(path, "+989121234567", "hi", None, "", thread_id, None, None)
                .unwrap();
        }
        let res = call(router_for("k", db), get("/api/v1/chats")).await;
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        let chats = v["chats"].as_array().unwrap();
        let find = |id: i64| chats.iter().find(|c| c["thread_id"] == id).unwrap();
        assert_eq!(find(1)["kind"], "general");
        assert_eq!(find(1)["sort"], Value::Null);
        assert_eq!(find(9)["kind"], "contact");
        assert_eq!(find(12)["kind"], "sorted");
        assert_eq!(find(12)["sort"], "promotional");
        assert_eq!(find(15)["kind"], "number");
    }

    #[tokio::test]
    async fn patterns_crud() {
        let app = router_for("k", sorting_db());
        let res = call(app.clone(), get("/api/v1/otp-patterns")).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_json(res).await, json!({"patterns": []}));

        let body = r#"{"name":"bank","pattern":"code\\s+(\\d{4,8})"}"#;
        let res = call(app.clone(), send_json("POST", "/api/v1/otp-patterns", body)).await;
        assert_eq!(res.status(), StatusCode::CREATED);
        let created = body_json(res).await;
        assert_eq!(created["name"], "bank");
        assert_eq!(created["pattern"], r"code\s+(\d{4,8})");
        assert!(created["created_at"].is_string());
        let id = created["id"].as_i64().unwrap();

        let res = call(app.clone(), send_json("POST", "/api/v1/otp-patterns", body)).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_json(res).await["id"], id);

        let res = call(app.clone(), get("/api/v1/otp-patterns")).await;
        let v = body_json(res).await;
        assert_eq!(v["patterns"].as_array().unwrap().len(), 1);
        assert_eq!(v["patterns"][0], created);

        let res = call(app.clone(), delete(&format!("/api/v1/otp-patterns/{id}"))).await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let res = call(app.clone(), delete(&format!("/api/v1/otp-patterns/{id}"))).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert_eq!(body_json(res).await["error"], "not_found");

        let res = call(app, get("/api/v1/otp-patterns")).await;
        assert_eq!(body_json(res).await, json!({"patterns": []}));
    }

    #[tokio::test]
    async fn posting_loose_pattern_is_rejected() {
        let app = router_for("k", sorting_db());
        let res = call(
            app,
            send_json(
                "POST",
                "/api/v1/otp-patterns",
                r#"{"pattern":":\\s+(\\d{4,8})"}"#,
            ),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let v = body_json(res).await;
        assert_eq!(v["error"], "validation");
        assert!(!v["message"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn learn_from_message_returns_updated_message() {
        let db = sorting_db();
        db.record_inbound(
            "/1",
            "+989121234567",
            "Your verification code 402332 for login",
            None,
            "",
            9,
            None,
            None,
        )
        .unwrap();
        let app = router_for("k", db.clone());
        let res = call(
            app.clone(),
            send_json(
                "POST",
                "/api/v1/otp-patterns/learn",
                r#"{"message_id":"in:1","code":"402332","name":"login"}"#,
            ),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["created"], true);
        assert_eq!(v["pattern"]["name"], "login");
        assert_eq!(v["message"]["id"], "in:1");
        assert_eq!(v["message"]["otp_code"], "402332");
        assert_eq!(db.otp_patterns().unwrap().len(), 1);

        let res = call(app, get("/api/v1/chats/9/messages")).await;
        assert_eq!(body_json(res).await["messages"][0]["otp_code"], "402332");
    }

    #[tokio::test]
    async fn learn_from_body_has_no_message() {
        let app = router_for("k", sorting_db());
        let res = call(
            app,
            send_json(
                "POST",
                "/api/v1/otp-patterns/learn",
                r#"{"body":"Your verification code 402332 for login","code":"402332"}"#,
            ),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["created"], true);
        assert_eq!(v["message"], Value::Null);
        assert!(v["pattern"]["id"].is_i64());
    }

    #[tokio::test]
    async fn learn_requires_exactly_one_source() {
        let app = router_for("k", sorting_db());
        let cases = [
            (r#"{"code":"402332"}"#, StatusCode::BAD_REQUEST),
            (
                r#"{"message_id":"in:1","body":"code 402332","code":"402332"}"#,
                StatusCode::BAD_REQUEST,
            ),
            (
                r#"{"message_id":"out:1","code":"402332"}"#,
                StatusCode::BAD_REQUEST,
            ),
            (
                r#"{"message_id":"nonsense","code":"402332"}"#,
                StatusCode::BAD_REQUEST,
            ),
            (
                r#"{"message_id":"in:999","code":"402332"}"#,
                StatusCode::NOT_FOUND,
            ),
            (r#"{"body":"code 402332"}"#, StatusCode::BAD_REQUEST),
        ];
        for (body, status) in cases {
            let res = call(
                app.clone(),
                send_json("POST", "/api/v1/otp-patterns/learn", body),
            )
            .await;
            assert_eq!(res.status(), status, "body: {body}");
        }
    }

    #[tokio::test]
    async fn otp_routes_require_api_key() {
        let app = router_for("k", sorting_db());
        for (method, uri) in [
            ("GET", "/api/v1/otp-patterns"),
            ("POST", "/api/v1/otp-patterns"),
            ("DELETE", "/api/v1/otp-patterns/1"),
            ("POST", "/api/v1/otp-patterns/learn"),
        ] {
            let res = call(
                app.clone(),
                axum::http::Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{method} {uri}");
        }
    }
}
