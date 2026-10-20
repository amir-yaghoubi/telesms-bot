//! Realtime WebSocket server.
//!
//! A second, opt-in transport next to the REST API: clients connect to this
//! port, authenticate with the same API key, and receive an event whenever a
//! message is recorded. Clients may also ask the gateway to send an SMS.
//!
//! Protocol (JSON text frames):
//!
//! ```text
//! server -> client  {"v":1,"type":"message.received","data":{…}}
//! client -> server  {"v":1,"type":"send_sms","id":"c1","data":{"number":"+98…","text":"hi"}}
//! server -> client  {"v":1,"type":"ack","id":"c1","data":{"e164":"+98…","thread_id":7,"sent":true}}
//! server -> client  {"v":1,"type":"error","id":"c1","error":"need_default_number",…}
//! ```
//!
//! Events are fire-and-forget: a client that cannot keep up is disconnected
//! rather than allowed to slow the gateway down, and resynchronizes by
//! reconnecting with `?after=<timestamp>`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Message, WebSocketConfig};
use tokio_util::sync::CancellationToken;

use crate::actions::{self, Identity};
use crate::app::AppState;
use crate::events::MessageEvent;
use crate::route::GENERAL_THREAD;

/// Version of the frame protocol.
pub const PROTOCOL_VERSION: u8 = 1;

/// Maximum number of simultaneously connected clients.
pub const MAX_CONNECTIONS: usize = 32;

/// Maximum size of a single inbound frame.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// How often the server pings a client.
pub const PING_INTERVAL: Duration = Duration::from_secs(30);

/// How long a client may stay silent before the connection is closed.
pub const PONG_DEADLINE: Duration = Duration::from_secs(60);

/// How many missed messages a reconnecting client may catch up on.
pub const REPLAY_LIMIT: i64 = 500;

/// Query parameters a client may send on the handshake.
#[derive(Debug, Default, Clone)]
struct ConnectParams {
    /// Cursor: deliver messages recorded after this timestamp first.
    after: Option<String>,
}

/// Read the query parameters of the handshake request.
fn parse_params(query: Option<&str>) -> ConnectParams {
    let mut params = ConnectParams::default();
    for pair in query.unwrap_or("").split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key == "after" {
            params.after = Some(percent_decode(value));
        }
    }
    params
}

/// Decode `%XX` escapes in a query value.
///
/// `+` is left alone: it is a valid character in a base64 API key.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The API key a client presented, if any.
fn presented_key(request: &Request) -> Option<String> {
    if let Some(value) = request.headers().get("X-Api-Key") {
        if let Ok(value) = value.to_str() {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    let query = request.uri().query()?;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key == "api_key" {
            let value = percent_decode(value);
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// A message as delivered to clients.
#[derive(Debug, Clone, Serialize)]
pub struct MessagePayload {
    pub id: String,
    pub direction: String,
    pub e164: String,
    pub body: String,
    pub timestamp: String,
    pub sms_ts: Option<String>,
    pub status: String,
    pub thread_id: i32,
}

impl From<&MessageEvent> for MessagePayload {
    fn from(message: &MessageEvent) -> Self {
        Self {
            id: message.id.clone(),
            direction: message.direction.clone(),
            e164: message.e164.clone(),
            body: message.body.clone(),
            timestamp: message.timestamp.clone(),
            sms_ts: message.sms_ts.clone(),
            status: message.status.clone(),
            thread_id: message.thread_id,
        }
    }
}

/// The event name a recorded message is announced under.
pub fn event_type(message: &MessageEvent) -> &'static str {
    match message.direction.as_str() {
        "out" => match message.status.as_str() {
            "ok" => "message.sent",
            _ => "message.failed",
        },
        _ => "message.received",
    }
}

/// Frame sent to clients for a recorded message.
pub fn event_frame(message: &MessageEvent) -> Value {
    json!({
        "v": PROTOCOL_VERSION,
        "type": event_type(message),
        "data": MessagePayload::from(message),
    })
}

/// A frame received from a client.
#[derive(Debug, Deserialize)]
struct CommandFrame {
    #[serde(default)]
    v: Option<u8>,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    data: Value,
}

/// Payload of a `send_sms` command.
#[derive(Debug, Deserialize)]
struct SendSmsData {
    #[serde(default)]
    number: Option<String>,
    #[serde(default)]
    contact_id: Option<i64>,
    #[serde(default)]
    thread_id: Option<i32>,
    #[serde(default)]
    text: String,
}

/// Frame answering a command.
fn ack_frame(id: Option<&str>, data: Value) -> Value {
    let mut frame = json!({"v": PROTOCOL_VERSION, "type": "ack"});
    if let Some(id) = id {
        frame["id"] = json!(id);
    }
    frame["data"] = data;
    frame
}

/// Frame reporting a failed command.
pub fn error_frame(id: Option<&str>, body: Value) -> Value {
    let mut frame = json!({"v": PROTOCOL_VERSION, "type": "error"});
    if let Some(id) = id {
        frame["id"] = json!(id);
    }
    if let Value::Object(fields) = body {
        for (key, value) in fields {
            frame[key] = value;
        }
    }
    frame
}

/// Frame reporting a protocol level failure.
fn protocol_error(id: Option<&str>, code: &str, message: &str) -> Value {
    error_frame(
        id,
        json!({
            "error": code,
            "message": message,
        }),
    )
}

/// Run the WebSocket server on `bind` until the cancellation token fires.
pub async fn serve(state: AppState, bind: SocketAddr, cancel: CancellationToken) {
    let listener = match TcpListener::bind(bind).await {
        Ok(listener) => listener,
        Err(err) => {
            tracing::error!(%bind, error = %err, "websocket bind failed");
            return;
        }
    };
    run(state, listener, cancel).await;
}

/// Accept clients until the cancellation token fires.
pub async fn run(state: AppState, listener: TcpListener, cancel: CancellationToken) {
    let bind = listener
        .local_addr()
        .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], 0)));
    tracing::info!(%bind, "websocket server listening");

    let connections = Arc::new(AtomicUsize::new(0));
    loop {
        let accepted = tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(err) => {
                tracing::warn!(error = %err, "websocket accept failed");
                continue;
            }
        };

        if connections.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
            tracing::warn!(%peer, "websocket connection refused: too many clients");
            reject_service_unavailable(stream).await;
            continue;
        }
        connections.fetch_add(1, Ordering::SeqCst);
        let guard = ConnectionGuard {
            connections: connections.clone(),
        };

        let state = state.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let _guard = guard;
            if let Err(err) = handle_connection(stream, state, cancel).await {
                tracing::debug!(%peer, error = %err, "websocket connection closed");
            }
        });
    }
}

/// Releases the connection slot when a connection ends.
struct ConnectionGuard {
    connections: Arc<AtomicUsize>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.connections.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Tell a client the server is at capacity.
async fn reject_service_unavailable(mut stream: TcpStream) {
    let body = "websocket server is at capacity\n";
    let response = format!(
        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

/// Accept one client and serve it until it goes away.
// The handshake callback returns tungstenite's own error response type, which
// is large by construction.
#[allow(clippy::result_large_err)]
async fn handle_connection(
    stream: TcpStream,
    state: AppState,
    cancel: CancellationToken,
) -> Result<(), String> {
    let expected = state
        .cfg
        .api_key
        .clone()
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| "api key not configured".to_string())?;

    let params: Arc<Mutex<ConnectParams>> = Arc::new(Mutex::new(ConnectParams::default()));
    let params_for_handshake = params.clone();
    let expected_for_handshake = expected.clone();

    let callback =
        move |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
            let presented = presented_key(request);
            let authorized = presented
                .as_deref()
                .is_some_and(|key| crate::http::api_key_matches(&expected_for_handshake, key));
            if !authorized {
                let mut rejection = tokio_tungstenite::tungstenite::http::Response::new(Some(
                    "Unauthorized".to_string(),
                ));
                *rejection.status_mut() = StatusCode::UNAUTHORIZED;
                return Err(rejection);
            }
            let parsed = parse_params(request.uri().query());
            if let Ok(mut guard) = params_for_handshake.lock() {
                *guard = parsed;
            }
            Ok(response)
        };

    let mut config = WebSocketConfig::default();
    config.max_frame_size = Some(MAX_FRAME_BYTES);
    config.max_message_size = Some(MAX_FRAME_BYTES);

    let websocket = tokio_tungstenite::accept_hdr_async_with_config(stream, callback, Some(config))
        .await
        .map_err(|err| format!("handshake failed: {err}"))?;

    let params = params.lock().map(|guard| guard.clone()).unwrap_or_default();

    // Subscribe before replaying, so nothing recorded in between is lost.
    let mut events = state.bus.subscribe();
    let mut replayed = std::collections::HashSet::new();

    let (mut sink, mut source) = websocket.split();

    if let Some(after) = params.after.as_deref() {
        match state.db.messages_after(after, REPLAY_LIMIT) {
            Ok(rows) => {
                for row in rows {
                    let message = MessageEvent::from(row);
                    replayed.insert(message.id.clone());
                    let frame = event_frame(&message);
                    sink.send(Message::Text(frame.to_string().into()))
                        .await
                        .map_err(|err| format!("replay send failed: {err}"))?;
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "websocket replay failed");
            }
        }
    }

    let mut ticker = tokio::time::interval(PING_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Consume the immediate first tick, so the first ping goes out one
    // interval after the connection was accepted.
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
            received = events.recv() => {
                match received {
                    Ok(message) => {
                        if replayed.contains(&message.id) {
                            continue;
                        }
                        let frame = event_frame(&message);
                        if sink.send(Message::Text(frame.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "websocket client lagged");
                        let _ = sink.send(Message::Close(Some(CloseFrame {
                            code: CloseCode::Again,
                            reason: "lagged".to_string().into(),
                        }))).await;
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            incoming = timeout(PONG_DEADLINE, source.next()) => {
                let Ok(Some(incoming)) = incoming else {
                    // Timed out or the peer went away.
                    let _ = sink.send(Message::Close(Some(CloseFrame {
                        code: CloseCode::Away,
                        reason: "no pong".to_string().into(),
                    }))).await;
                    break;
                };
                match incoming {
                    Ok(Message::Text(text)) => {
                        let frame = handle_command(&state, &text).await;
                        if sink.send(Message::Text(frame.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    Ok(Message::Binary(_)) => {
                        let frame = protocol_error(
                            None,
                            "unsupported_frame",
                            "binary frames are not supported",
                        );
                        if sink.send(Message::Text(frame.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(err) => {
                        tracing::debug!(error = %err, "websocket read failed");
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Handle a command frame and return the frame to answer with.
async fn handle_command(state: &AppState, text: &str) -> Value {
    let command: CommandFrame = match serde_json::from_str(text) {
        Ok(command) => command,
        Err(err) => {
            return protocol_error(None, "invalid_frame", &format!("invalid json frame: {err}"));
        }
    };
    let id = command.id.as_deref();

    if let Some(version) = command.v {
        if version != PROTOCOL_VERSION {
            return protocol_error(
                id,
                "unsupported_version",
                &format!("unsupported protocol version {version}"),
            );
        }
    }

    match command.kind.as_str() {
        "send_sms" => send_sms(state, id, command.data).await,
        other => protocol_error(id, "unknown_command", &format!("unknown command: {other}")),
    }
}

/// Run a `send_sms` command.
async fn send_sms(state: &AppState, id: Option<&str>, data: Value) -> Value {
    let payload: SendSmsData = match serde_json::from_value(data) {
        Ok(payload) => payload,
        Err(err) => {
            return protocol_error(
                id,
                "validation",
                &format!("invalid send_sms payload: {err}"),
            );
        }
    };

    let identity: Identity =
        match actions::identity_from_parts(payload.number, payload.contact_id, payload.thread_id) {
            Ok(identity) => identity,
            Err(err) => return error_frame(id, err.error_body()),
        };

    let reply_thread = identity.thread_id.unwrap_or(GENERAL_THREAD);
    let ctx = crate::app::SendContext {
        modem: state.modem.as_ref(),
        tg: state.tg.as_ref(),
        bus: &state.bus,
        delete_enabled: state.cfg.sms_delete_enabled,
    };
    match actions::send_sms(
        state.db.as_ref(),
        &state.cfg.default_region,
        &identity,
        &payload.text,
        reply_thread,
        None,
        &ctx,
    )
    .await
    {
        Ok(sent) => ack_frame(
            id,
            json!({
                "e164": sent.e164,
                "thread_id": sent.thread_id,
                "sent": sent.sent,
            }),
        ),
        Err(err) => error_frame(id, err.error_body()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::FakeTg;
    use crate::db::Topic;
    use crate::events::EventBus;
    use crate::modem::FakeModem;
    use std::path::PathBuf;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    fn test_config(api_key: &str) -> crate::config::Config {
        crate::config::Config {
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

    fn test_state(api_key: &str) -> (AppState, Arc<FakeModem>, EventBus) {
        let db = Arc::new({
            let db = crate::db::Db::open_in_memory().unwrap();
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
        let bus = EventBus::new(crate::events::EVENT_BUFFER);
        let state = AppState {
            cfg: test_config(api_key),
            db,
            modem: modem.clone(),
            info: Arc::new(FakeModem::default()),
            forward: Arc::new(FakeModem::default()),
            tg: Arc::new(FakeTg::new()),
            bus: bus.clone(),
        };
        (state, modem, bus)
    }

    fn recorded(direction: &str, status: &str) -> MessageEvent {
        MessageEvent {
            id: "in:1".into(),
            direction: direction.into(),
            e164: "+989121234567".into(),
            body: "hello".into(),
            timestamp: "2026-10-20T10:00:00+00:00".into(),
            sms_ts: None,
            status: status.into(),
            thread_id: 9,
        }
    }

    #[test]
    fn event_type_follows_direction_and_status() {
        assert_eq!(event_type(&recorded("in", "ok")), "message.received");
        assert_eq!(event_type(&recorded("out", "ok")), "message.sent");
        assert_eq!(event_type(&recorded("out", "failed")), "message.failed");
    }

    #[test]
    fn event_frame_carries_version_type_and_payload() {
        let frame = event_frame(&recorded("in", "ok"));
        assert_eq!(frame["v"], PROTOCOL_VERSION);
        assert_eq!(frame["type"], "message.received");
        assert_eq!(frame["data"]["id"], "in:1");
        assert_eq!(frame["data"]["thread_id"], 9);
        assert_eq!(frame["data"]["direction"], "in");
    }

    #[test]
    fn error_frame_keeps_the_transport_error_vocabulary() {
        let err = crate::actions::ActionError::NeedDefaultNumber {
            numbers: vec!["+989120000000".into()],
        };
        let frame = error_frame(Some("c1"), err.error_body());
        assert_eq!(frame["v"], PROTOCOL_VERSION);
        assert_eq!(frame["type"], "error");
        assert_eq!(frame["id"], "c1");
        assert_eq!(frame["error"], "need_default_number");
        assert_eq!(frame["numbers"][0], "+989120000000");
    }

    #[test]
    fn params_are_read_from_the_query() {
        let params = parse_params(Some("after=2026-10-20T10%3A00%3A00%2B00%3A00&x=1"));
        assert_eq!(params.after.as_deref(), Some("2026-10-20T10:00:00+00:00"));
    }

    #[test]
    fn percent_decoding_keeps_plus_signs() {
        assert_eq!(percent_decode("a+b%2Fc"), "a+b/c");
    }

    #[tokio::test]
    async fn unknown_command_is_reported() {
        let (state, _modem, _bus) = test_state("secret");
        let frame = handle_command(
            &state,
            &json!({"v": 1, "type": "nope", "id": "c9"}).to_string(),
        )
        .await;
        assert_eq!(frame["type"], "error");
        assert_eq!(frame["error"], "unknown_command");
        assert_eq!(frame["id"], "c9");
    }

    #[tokio::test]
    async fn unsupported_version_is_reported() {
        let (state, _modem, _bus) = test_state("secret");
        let frame = handle_command(
            &state,
            &json!({"v": 99, "type": "send_sms", "id": "c9"}).to_string(),
        )
        .await;
        assert_eq!(frame["error"], "unsupported_version");
    }

    #[tokio::test]
    async fn thread_only_identity_is_rejected() {
        let (state, _modem, _bus) = test_state("secret");
        let frame = handle_command(
            &state,
            &json!({"v": 1, "type": "send_sms", "id": "c1", "data": {"thread_id": 9, "text": "hi"}})
                .to_string(),
        )
        .await;
        assert_eq!(frame["error"], "missing_identity");
    }

    #[tokio::test]
    async fn send_sms_acknowledges_the_command() {
        let (state, _modem, _bus) = test_state("secret");
        let frame = handle_command(
            &state,
            &json!({"v": 1, "type": "send_sms", "id": "c1", "data": {"number": "+989121234567", "text": "hi"}})
                .to_string(),
        )
        .await;
        assert_eq!(frame["type"], "ack");
        assert_eq!(frame["id"], "c1");
        assert_eq!(frame["data"]["sent"], true);
        assert_eq!(frame["data"]["e164"], "+989121234567");
    }

    /// Start a server on an ephemeral port.
    async fn start_server(
        state: AppState,
    ) -> (SocketAddr, CancellationToken, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cancel = CancellationToken::new();
        let cancel_run = cancel.clone();
        let handle = tokio::spawn(async move {
            run(state, listener, cancel_run).await;
        });
        (addr, cancel, handle)
    }

    async fn connect(
        addr: SocketAddr,
        key: Option<&str>,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        tokio_tungstenite::tungstenite::Error,
    > {
        let stream = TcpStream::connect(addr).await.expect("connect");
        let mut request = format!("ws://{addr}/")
            .into_client_request()
            .expect("request");
        if let Some(key) = key {
            request
                .headers_mut()
                .insert("X-Api-Key", key.parse().expect("header value"));
        }
        tokio_tungstenite::client_async(request, stream)
            .await
            .map(|(ws, _)| ws)
    }

    /// Read the next text frame, skipping protocol frames such as pings.
    async fn next_text(
        client: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) -> Value {
        loop {
            let frame = timeout(Duration::from_secs(5), client.next())
                .await
                .expect("frame arrives")
                .expect("frame")
                .expect("websocket message");
            match frame {
                Message::Text(text) => {
                    return serde_json::from_str(&text).expect("json frame");
                }
                Message::Close(_) => panic!("connection closed"),
                _ => continue,
            }
        }
    }

    #[tokio::test]
    async fn handshake_without_a_key_is_rejected() {
        let (state, _modem, _bus) = test_state("secret");
        let (addr, cancel, server) = start_server(state).await;

        let result = connect(addr, None).await;
        assert!(result.is_err(), "handshake without a key must fail");
        cancel.cancel();
        server.abort();
    }

    #[tokio::test]
    async fn handshake_with_a_wrong_key_is_rejected() {
        let (state, _modem, _bus) = test_state("secret");
        let (addr, cancel, server) = start_server(state).await;

        let result = connect(addr, Some("wrong")).await;
        assert!(result.is_err(), "handshake with a wrong key must fail");
        cancel.cancel();
        server.abort();
    }

    #[tokio::test]
    async fn handshake_with_the_api_key_succeeds() {
        let (state, _modem, _bus) = test_state("secret");
        let (addr, cancel, server) = start_server(state).await;

        let client = connect(addr, Some("secret")).await;
        assert!(client.is_ok(), "handshake with the api key must succeed");
        cancel.cancel();
        server.abort();
    }

    #[tokio::test]
    async fn client_receives_announced_messages() {
        let (state, _modem, bus) = test_state("secret");
        let (addr, cancel, server) = start_server(state).await;

        let mut client = connect(addr, Some("secret")).await.expect("connect");

        bus.publish(recorded("in", "ok"));

        let value = next_text(&mut client).await;
        assert_eq!(value["type"], "message.received");
        assert_eq!(value["data"]["body"], "hello");
        assert_eq!(value["data"]["thread_id"], 9);

        cancel.cancel();
        server.abort();
    }

    #[tokio::test]
    async fn client_can_send_an_sms_over_the_socket() {
        let (state, modem, _bus) = test_state("secret");
        let (addr, cancel, server) = start_server(state).await;

        let mut client = connect(addr, Some("secret")).await.expect("connect");
        client
            .send(Message::Text(
                json!({"v": 1, "type": "send_sms", "id": "c1", "data": {"number": "+989121234567", "text": "hi"}})
                    .to_string()
                    .into(),
            ))
            .await
            .expect("send command");

        let value = next_text(&mut client).await;
        assert_eq!(value["type"], "ack");
        assert_eq!(value["id"], "c1");
        assert_eq!(value["data"]["sent"], true);

        let sent = modem.sent.lock().expect("modem lock").clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1, "hi");

        cancel.cancel();
        server.abort();
    }

    #[tokio::test]
    async fn reconnecting_client_replays_missed_messages() {
        let (state, _modem, _bus) = test_state("secret");
        let db = state.db.clone();
        let (addr, cancel, server) = start_server(state).await;

        db.record_inbound(
            "/sms/1",
            "+989121234567",
            "older",
            None,
            "2026-10-20T10:00:00+00:00",
            9,
            None,
            None,
        )
        .unwrap();

        // A cursor before the recorded message replays it on connect.
        let stream = TcpStream::connect(addr).await.expect("connect");
        let mut request = format!("ws://{addr}/?after=2026-10-20T09:00:00%2B00:00")
            .into_client_request()
            .expect("request");
        request
            .headers_mut()
            .insert("X-Api-Key", "secret".parse().expect("header value"));
        let (mut client, _) = tokio_tungstenite::client_async(request, stream)
            .await
            .expect("connect");

        let value = next_text(&mut client).await;
        assert_eq!(value["type"], "message.received");
        assert_eq!(value["data"]["body"], "older");

        cancel.cancel();
        server.abort();
    }
}
