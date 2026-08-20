use std::sync::Arc;
use std::time::Duration;

use teloxide::types::ChatId;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use telesms_bot::app::{sweep_old_sms, watch_inbox, watch_modem, TelegramSink};
use telesms_bot::config::Config;
use telesms_bot::db::Db;
use telesms_bot::google::GooglePeople;
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
    let db = Arc::new(Db::open(&cfg.database_path).expect("db"));
    let cancel = CancellationToken::new();

    let people = Arc::new(GooglePeople::new(
        cfg.google_token_path.clone(),
        cfg.google_client_id.clone(),
        cfg.google_client_secret.clone(),
    ));

    let mut tasks = JoinSet::new();

    let db_sync = db.clone();
    let region_sync = cfg.default_region.clone();
    let sync_every = cfg.contacts_sync_interval;
    let cancel_sync = cancel.clone();
    tasks.spawn(async move {
        sync_contacts(&people, &db_sync, &region_sync).await;
        let mut ticker = tokio::time::interval(sync_every);
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = cancel_sync.cancelled() => return,
                _ = ticker.tick() => {
                    sync_contacts(&people, &db_sync, &region_sync).await;
                }
            }
        }
    });

    let mm = Arc::new(
        MmModem::connect_with_uid(cfg.modem_uid.clone())
            .await
            .expect("modemmanager dbus"),
    );
    let modem: Arc<dyn SmsModem> = mm.clone();

    let tg: Arc<dyn TelegramSink> = Arc::new(RealTg {
        bot: teloxide::Bot::new(cfg.telegram_bot_token.clone()),
        chat_id: ChatId(cfg.telegram_group_id),
    });

    let db_in = db.clone();
    let region_in = cfg.default_region.clone();
    let tg_in = tg.clone();
    let mm_in = mm.clone();
    let delete_in = cfg.sms_delete_enabled;
    let cancel_in = cancel.clone();
    tasks.spawn(watch_inbox(
        mm_in,
        db_in,
        region_in,
        tg_in,
        delete_in,
        cancel_in,
        Duration::from_secs(5),
    ));

    let info: Arc<dyn telesms_bot::modem::ModemInfo> = mm.clone();
    let forward: Arc<dyn CallForward> = mm.clone();

    if cfg.api_enabled() {
        let bind: std::net::SocketAddr = format!("{}:{}", cfg.api_bind, cfg.api_port)
            .parse()
            .expect("API_BIND/API_PORT");
        let state = telesms_bot::http::HttpState {
            cfg: cfg.clone(),
            db: db.clone(),
            modem: modem.clone(),
            info: info.clone(),
            forward: forward.clone(),
            tg: tg.clone(),
        };
        let cancel_http = cancel.clone();
        tasks.spawn(async move {
            tracing::info!(%bind, "http api listening");
            telesms_bot::http::serve(state, bind, cancel_http).await;
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

    telegram::dispatch(cfg, db, modem, info, forward).await;
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
}

async fn sync_contacts(people: &GooglePeople, db: &Db, region: &str) {
    match people.sync_all(db, region).await {
        Ok(n) => {
            tracing::info!(contacts = n, "google contacts synced");
            db.set_contacts_available(true);
        }
        Err(err) => {
            tracing::error!(error = %err, "google contacts sync failed");
            db.set_contacts_available(false);
        }
    }
}
