use std::path::{Path, PathBuf};
use std::time::Duration;

use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge, RedirectUrl,
    Scope, TokenResponse, TokenUrl,
};
use serde::Deserialize;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::db::Db;
use crate::normalize::normalize_e164;

pub const CONTACTS_SCOPE: &str = "https://www.googleapis.com/auth/contacts.readonly";
pub const REDIRECT_URI: &str = "http://127.0.0.1:8765/";

async fn ensure_success(resp: reqwest::Response) -> Result<reqwest::Response, GoogleError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let url = resp.url().clone();
    let body = resp.text().await.unwrap_or_default();
    let snippet: String = body.chars().take(500).collect();
    Err(GoogleError::Other(format!("{status} {url} {snippet}")))
}

#[derive(Debug, Error)]
pub enum GoogleError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("db: {0}")]
    Db(#[from] crate::db::DbError),
    #[error("{0}")]
    Other(String),
}

pub struct GooglePeople {
    client: reqwest::Client,
    token_path: PathBuf,
    client_id: String,
    client_secret: String,
}

#[derive(Deserialize)]
struct PeopleList {
    #[serde(default)]
    connections: Vec<Person>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct Person {
    #[serde(rename = "resourceName")]
    resource_name: Option<String>,
    #[serde(default)]
    names: Vec<PersonName>,
    #[serde(rename = "phoneNumbers", default)]
    phone_numbers: Vec<PhoneNumber>,
}

#[derive(Deserialize)]
struct PersonName {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct PhoneNumber {
    value: Option<String>,
}

#[derive(Deserialize)]
struct StoredToken {
    refresh_token: String,
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: String,
}

type ParsedContact = (String, String, Vec<String>);

impl GooglePeople {
    pub fn new(token_path: PathBuf, client_id: String, client_secret: String) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .expect("reqwest client"),
            token_path,
            client_id,
            client_secret,
        }
    }

    pub async fn access_token(&self) -> Result<String, GoogleError> {
        let raw = tokio::fs::read_to_string(&self.token_path).await?;
        let stored: StoredToken = serde_json::from_str(&raw)?;
        let resp = self
            .client
            .post("https://oauth2.googleapis.com/token")
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("refresh_token", stored.refresh_token.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await?;
        let resp = ensure_success(resp).await?;
        let body: RefreshResponse = resp.json().await?;
        Ok(body.access_token)
    }

    pub async fn sync_all(&self, db: &Db, region: &str) -> Result<usize, GoogleError> {
        let token = self.access_token().await?;
        let mut page_token: Option<String> = None;
        let mut all = Vec::new();
        loop {
            let mut req = self
                .client
                .get("https://people.googleapis.com/v1/people/me/connections")
                .bearer_auth(&token)
                .query(&[("personFields", "names,phoneNumbers"), ("pageSize", "1000")]);
            if let Some(pt) = &page_token {
                req = req.query(&[("pageToken", pt.as_str())]);
            }
            let resp = ensure_success(req.send().await?).await?;
            let body = resp.text().await?;
            let (page, next) = parse_people_page(&body, region)?;
            all.extend(page);
            match next {
                Some(t) if !t.is_empty() => page_token = Some(t),
                _ => break,
            }
        }
        sync_parsed(db, all)
    }
}

pub fn contacts_from_people_json(
    body: &str,
    region: &str,
) -> Result<Vec<ParsedContact>, GoogleError> {
    Ok(parse_people_page(body, region)?.0)
}

fn parse_people_page(
    body: &str,
    region: &str,
) -> Result<(Vec<ParsedContact>, Option<String>), GoogleError> {
    let parsed: PeopleList = serde_json::from_str(body)?;
    let contacts = parsed
        .connections
        .into_iter()
        .filter_map(|p| person_to_contact(p, region))
        .collect();
    Ok((contacts, parsed.next_page_token))
}

fn person_to_contact(p: Person, region: &str) -> Option<ParsedContact> {
    let resource = p.resource_name?;
    let display = p
        .names
        .first()
        .and_then(|n| n.display_name.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Unknown".to_string());
    let mut numbers = Vec::new();
    for raw in p.phone_numbers.into_iter().filter_map(|ph| ph.value) {
        let Ok(e164) = normalize_e164(&raw, region) else {
            continue;
        };
        if !numbers.contains(&e164) {
            numbers.push(e164);
        }
    }
    Some((resource, display, numbers))
}

pub fn sync_parsed(db: &Db, contacts: Vec<ParsedContact>) -> Result<usize, GoogleError> {
    let n = contacts.len();
    for (resource, name, numbers) in contacts {
        let id = db.upsert_contact(&resource, &name)?;
        db.replace_contact_numbers(id, &numbers)?;
    }
    Ok(n)
}

pub async fn auth_url_and_listen(
    client_id: &str,
    client_secret: &str,
    token_path: &Path,
) -> Result<(), GoogleError> {
    let client = BasicClient::new(ClientId::new(client_id.to_string()))
        .set_client_secret(ClientSecret::new(client_secret.to_string()))
        .set_auth_uri(
            AuthUrl::new("https://accounts.google.com/o/oauth2/v2/auth".to_string())
                .map_err(|e| GoogleError::Other(e.to_string()))?,
        )
        .set_token_uri(
            TokenUrl::new("https://oauth2.googleapis.com/token".to_string())
                .map_err(|e| GoogleError::Other(e.to_string()))?,
        )
        .set_redirect_uri(
            RedirectUrl::new(REDIRECT_URI.to_string())
                .map_err(|e| GoogleError::Other(e.to_string()))?,
        );

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let (auth_url, csrf) = client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new(CONTACTS_SCOPE.to_string()))
        .add_extra_param("access_type", "offline")
        .add_extra_param("prompt", "consent")
        .set_pkce_challenge(pkce_challenge)
        .url();

    println!("Open this URL in your browser:\n{auth_url}\n");

    let (code, state) = listen_for_redirect().await?;
    if state.secret() != csrf.secret() {
        return Err(GoogleError::Other("oauth state mismatch".into()));
    }

    let http = reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let token = client
        .exchange_code(code)
        .set_pkce_verifier(pkce_verifier)
        .request_async(&http)
        .await
        .map_err(|e| GoogleError::Other(e.to_string()))?;
    let refresh = token
        .refresh_token()
        .ok_or_else(|| GoogleError::Other("Google did not return a refresh_token".into()))?;

    let stored = serde_json::json!({
        "refresh_token": refresh.secret(),
        "token_type": "Bearer",
    });
    if let Some(parent) = token_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(token_path, serde_json::to_vec_pretty(&stored)?)?;
    println!("Wrote refresh token to {}", token_path.display());
    Ok(())
}

async fn listen_for_redirect() -> Result<(AuthorizationCode, CsrfToken), GoogleError> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8765").await?;
    let (mut stream, _) = listener.accept().await?;
    let mut buf = vec![0u8; 8192];
    let n = stream.read(&mut buf).await?;
    let req = String::from_utf8_lossy(&buf[..n]);
    let request_line = req.lines().next().unwrap_or("");
    let path = request_line.split_whitespace().nth(1).unwrap_or("/");
    let url = oauth2::url::Url::parse(&format!("http://127.0.0.1{path}"))
        .map_err(|e| GoogleError::Other(e.to_string()))?;
    let code = url
        .query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| AuthorizationCode::new(v.into_owned()))
        .ok_or_else(|| GoogleError::Other("redirect missing code".into()))?;
    let state = url
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| CsrfToken::new(v.into_owned()))
        .ok_or_else(|| GoogleError::Other("redirect missing state".into()))?;
    let message = "You can close this tab.";
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\n\r\n{}",
        message.len(),
        message
    );
    stream.write_all(response.as_bytes()).await?;
    Ok((code, state))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_people_connection() {
        let j = r#"{"connections":[{"resourceName":"people/x","names":[{"displayName":"Ali"}],"phoneNumbers":[{"value":"09120000000"}]}]}"#;
        let v = contacts_from_people_json(j, "IR").unwrap();
        assert_eq!(v[0].0, "people/x");
        assert_eq!(v[0].1, "Ali");
        assert_eq!(v[0].2, vec!["+989120000000".to_string()]);
    }

    #[test]
    fn parse_people_dedupes_normalized_numbers() {
        let j = r#"{"connections":[{"resourceName":"people/x","names":[{"displayName":"Ali"}],"phoneNumbers":[{"value":"09120000000"},{"value":"+989120000000"}]}]}"#;
        let v = contacts_from_people_json(j, "IR").unwrap();
        assert_eq!(v[0].2, vec!["+989120000000".to_string()]);
        let db = Db::open_in_memory().unwrap();
        assert_eq!(sync_parsed(&db, v).unwrap(), 1);
        assert_eq!(
            db.search_contacts("ali").unwrap()[0].numbers,
            vec!["+989120000000".to_string()]
        );
    }

    #[test]
    fn parse_people_unknown_name_skips_invalid_numbers() {
        let j = r#"{"connections":[{"resourceName":"people/y","phoneNumbers":[{"value":"not-a-number"},{"value":"+14155552671"}]}]}"#;
        let v = contacts_from_people_json(j, "IR").unwrap();
        assert_eq!(v[0].0, "people/y");
        assert_eq!(v[0].1, "Unknown");
        assert_eq!(v[0].2, vec!["+14155552671".to_string()]);
    }

    #[test]
    fn sync_parsed_upserts() {
        let db = Db::open_in_memory().unwrap();
        let n = sync_parsed(
            &db,
            vec![("people/x".into(), "Ali".into(), vec!["+98912".into()])],
        )
        .unwrap();
        assert_eq!(n, 1);
        assert!(db.search_contacts("ali").unwrap().len() == 1);
    }

    #[test]
    fn auth_url_includes_scope_and_redirect() {
        let client = BasicClient::new(ClientId::new("cid".into()))
            .set_auth_uri(
                AuthUrl::new("https://accounts.google.com/o/oauth2/v2/auth".into()).unwrap(),
            )
            .set_redirect_uri(RedirectUrl::new(REDIRECT_URI.into()).unwrap());
        let (url, _) = client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new(CONTACTS_SCOPE.into()))
            .url();
        let s = url.to_string();
        assert!(s.contains("contacts.readonly"));
        assert!(s.contains("127.0.0.1"));
        assert!(s.contains("8765"));
        assert_eq!(REDIRECT_URI, "http://127.0.0.1:8765/");
        assert_eq!(
            CONTACTS_SCOPE,
            "https://www.googleapis.com/auth/contacts.readonly"
        );
    }
}
