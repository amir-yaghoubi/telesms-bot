use std::sync::Arc;
use std::time::Duration;

use teloxide::types::ChatId;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use telesms_bot::app::{sweep_old_sms, watch_contacts, watch_inbox, watch_modem, TelegramSink};
use telesms_bot::config::Config;
use telesms_bot::db::Db;
use telesms_bot::google::{ContactsSync, GooglePeople};
use telesms_bot::modem::{CallForward, SmsModem};
use telesms_bot::modem_mm::MmModem;
use telesms_bot::telegram::{self, RealTg};

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    match std::env::args().nth(1).as_deref() {
        Some("check-modem") => check_modem().await,
        Some("auth") => google_auth().await,
        Some("classify") => classify_cli().await,
        _ => run_daemon().await,
    }
}

async fn google_auth() {
    let client_id = std::env::var("GOOGLE_CLIENT_ID").expect("GOOGLE_CLIENT_ID");
    let client_secret = std::env::var("GOOGLE_CLIENT_SECRET").expect("GOOGLE_CLIENT_SECRET");
    let token_path = std::env::var("GOOGLE_TOKEN_PATH")
        .unwrap_or_else(|_| "./secrets/google-token.json".to_string());
    telesms_bot::google::auth_url_and_listen(
        &client_id,
        &client_secret,
        std::path::Path::new(&token_path),
    )
    .await
    .expect("auth");
}

/// An env var that is set and not blank.
fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

async fn classify_cli() {
    use telesms_bot::sorting::sweep::{classify_pending, CliMode, SweepRun};

    let mode = match std::env::args().nth(2).as_deref() {
        None | Some("--missing") => CliMode::Missing,
        Some("--failed") => CliMode::Failed,
        Some("--force") => CliMode::Force,
        Some(other) => {
            eprintln!("unknown option {other}; usage: classify [--missing | --failed | --force]");
            std::process::exit(2);
        }
    };
    let url = env_opt("CLASSIFIER_URL");
    let model = env_opt("CLASSIFIER_MODEL");
    let endpoint = url.as_deref().zip(model.as_deref());
    let config_path = env_opt("SORTING_CONFIG_PATH").map(std::path::PathBuf::from);
    let sorting = telesms_bot::sorting::Sorting::build(endpoint, config_path.as_deref())
        .unwrap_or_else(|err| {
            eprintln!("sorting config: {err}");
            std::process::exit(1);
        });
    let Some(classifier) = sorting.classifier.clone() else {
        eprintln!("classifier disabled: set CLASSIFIER_URL and CLASSIFIER_MODEL");
        std::process::exit(1);
    };
    let database_path =
        env_opt("DATABASE_PATH").unwrap_or_else(|| "./data/telesms.sqlite".to_string());
    let db = Db::open(std::path::Path::new(&database_path)).expect("db");
    let report = classify_pending(
        &db,
        classifier.as_ref(),
        None,
        &SweepRun::cli(mode),
        sorting.config.sweep.max_attempts,
    )
    .await
    .expect("classify");
    println!("classified {}, failed {}", report.classified, report.failed);
    if report.stopped_by_breaker {
        eprintln!("classifier unreachable; stopped early");
    }
}

async fn check_modem() {
    let mm = telesms_bot::modem_mm::MmModem::connect()
        .await
        .expect("connect");
    let path = mm.resolve_path().await.expect("resolve");
    println!("{path}");
}

async fn run_daemon() {
    let cfg = Config::from_env().expect("config");
    if let Some(parent) = cfg.database_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).expect("create database directory");
        }
    }
    let sorting =
        telesms_bot::sorting::Sorting::build(cfg.classifier(), cfg.sorting_config_path.as_deref())
            .unwrap_or_else(|err| {
                eprintln!("sorting config: {err}");
                std::process::exit(1);
            });
    match cfg.classifier() {
        Some((url, model)) => tracing::info!(url, model, "classifier enabled"),
        None => tracing::info!("classifier disabled"),
    }
    let db = Arc::new(Db::open(&cfg.database_path).expect("db"));
    let bus = telesms_bot::events::EventBus::new(telesms_bot::events::EVENT_BUFFER);
    let cancel = CancellationToken::new();

    let people: Arc<dyn ContactsSync> = Arc::new(GooglePeople::new(
        cfg.google_token_path.clone(),
        cfg.google_client_id.clone(),
        cfg.google_client_secret.clone(),
    ));

    let tg: Arc<dyn TelegramSink> = Arc::new(RealTg {
        bot: teloxide::Bot::new(cfg.telegram_bot_token.clone()),
        chat_id: ChatId(cfg.telegram_group_id),
    });

    let mut tasks = JoinSet::new();

    let db_sync = db.clone();
    let region_sync = cfg.default_region.clone();
    let sync_every = cfg.contacts_sync_interval;
    let cancel_sync = cancel.clone();
    tasks.spawn(watch_contacts(
        people,
        db_sync,
        region_sync,
        tg.clone(),
        sync_every,
        vec![Duration::from_secs(30), Duration::from_secs(120)],
        cancel_sync,
    ));

    let mm = Arc::new(
        MmModem::connect_with_uid(cfg.modem_uid.clone())
            .await
            .expect("modemmanager dbus"),
    );
    let modem: Arc<dyn SmsModem> = mm.clone();

    let watch = telesms_bot::app::InboxWatch {
        db: db.clone(),
        region: cfg.default_region.clone(),
        tg: tg.clone(),
        delete_enabled: cfg.sms_delete_enabled,
        bus: bus.clone(),
        sorting: sorting.clone(),
    };
    let mm_in = mm.clone();
    let cancel_in = cancel.clone();
    tasks.spawn(watch_inbox(mm_in, watch, cancel_in, Duration::from_secs(5)));

    let info: Arc<dyn telesms_bot::modem::ModemInfo> = mm.clone();
    let forward: Arc<dyn CallForward> = mm.clone();

    let state = telesms_bot::app::AppState {
        cfg: cfg.clone(),
        db: db.clone(),
        modem: modem.clone(),
        info: info.clone(),
        forward: forward.clone(),
        tg: tg.clone(),
        bus: bus.clone(),
    };

    if cfg.api_enabled() {
        let bind: std::net::SocketAddr = format!("{}:{}", cfg.api_bind, cfg.api_port)
            .parse()
            .expect("API_BIND/API_PORT");
        let state = state.clone();
        let cancel_http = cancel.clone();
        tasks.spawn(async move {
            tracing::info!(%bind, "http api listening");
            telesms_bot::http::serve(state, bind, cancel_http).await;
        });
    }

    if cfg.ws_enabled && !cfg.api_enabled() {
        tracing::warn!("WS_ENABLED is set but API_KEY is missing; websocket server not started");
    } else if cfg.ws_server_enabled() {
        let bind: std::net::SocketAddr = format!("{}:{}", cfg.ws_bind, cfg.ws_port)
            .parse()
            .expect("WS_BIND/WS_PORT");
        let state = state.clone();
        let cancel_ws = cancel.clone();
        tasks.spawn(async move {
            telesms_bot::ws::serve(state, bind, cancel_ws).await;
        });
    }

    tasks.spawn(watch_modem(
        info.clone(),
        tg.clone(),
        Duration::from_secs(5),
        cancel.clone(),
    ));

    if cfg.sms_delete_enabled {
        let mm_sw = mm.clone();
        let db_sw = db.clone();
        let max_age = cfg.sms_delete_max_age;
        let cancel_sw = cancel.clone();
        tasks.spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(6 * 3600));
            loop {
                tokio::select! {
                    _ = cancel_sw.cancelled() => return,
                    _ = ticker.tick() => {
                        if let Err(err) = sweep_old_sms(&mm_sw, &db_sw, max_age).await {
                            tracing::warn!(error = %err, "sms sweep failed");
                        }
                    }
                }
            }
        });
    }

    tasks.spawn(telesms_bot::sorting::sweep::run_sweep(
        db.clone(),
        sorting.clone(),
        bus.clone(),
        cancel.clone(),
    ));

    let send = telesms_bot::app::SendDeps {
        modem: modem.clone(),
        bus: bus.clone(),
    };
    telegram::dispatch(cfg, db, send, info, forward, sorting.config.clone()).await;
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
}
