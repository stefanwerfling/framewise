//! Minimal Jellyfin API client (read only).

use std::time::Duration;

use reqwest::StatusCode;
use serde::de::DeserializeOwned;

use super::dto::{
    AuthenticationResult, ItemsResult, QuickConnectResult, SystemInfo, User, VirtualFolder,
};
use crate::{tr, trf};

const TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Items per request. Items carry all streams, so pages stay moderate.
pub const PAGE_SIZE: usize = 200;
const FIELDS: &str = "MediaSources,Path,ProductionYear,DateCreated";

/// Why a request failed – `message` is meant for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientError {
    pub message: String,
    /// HTTP status if the server answered.
    pub status: Option<u16>,
}

impl ClientError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: None,
        }
    }
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ClientError {}

pub type Result<T> = std::result::Result<T, ClientError>;

#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    auth: String,
    /// User of a Quick Connect login.
    user: Option<String>,
    /// Set once `/Items` was rejected without a user (servers before 10.9).
    legacy_user: Option<String>,
}

impl Client {
    /// `token` may be empty before a login; `device_id` empty = "framewise".
    pub fn new(url: &str, token: &str, device_id: &str) -> Result<Self> {
        let base = url.trim().trim_end_matches('/').to_string();
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(ClientError::new(tr!(
                "The server address must start with http:// or https://"
            )));
        }
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("framewise/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| ClientError::new(e.to_string()))?;
        let device_id = match device_id.trim() {
            "" => "framewise",
            id => id,
        };
        let mut auth = format!(
            "MediaBrowser Client=\"framewise\", Device=\"framewise\", DeviceId=\"{device_id}\", Version=\"{}\"",
            env!("CARGO_PKG_VERSION"),
        );
        if !token.trim().is_empty() {
            auth.push_str(&format!(", Token=\"{}\"", token.trim()));
        }
        Ok(Self {
            http,
            base,
            auth,
            user: None,
            legacy_user: None,
        })
    }

    /// Read as this user (Quick Connect login) instead of with an API key.
    pub fn with_user(mut self, user_id: &str) -> Self {
        let id = user_id.trim();
        self.user = (!id.is_empty()).then(|| id.to_string());
        self
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
        self.send(self.http.get(format!("{}{path}", self.base)).query(query))
            .await
    }

    pub async fn system_info(&self) -> Result<SystemInfo> {
        self.get("/System/Info", &[]).await
    }

    /// All libraries with their folders. Needs an administrator.
    pub async fn libraries(&self) -> Result<Vec<VirtualFolder>> {
        self.get("/Library/VirtualFolders", &[]).await
    }

    /// The libraries the logged-in user sees (no folders). Works without
    /// administrator rights.
    pub async fn user_views(&self) -> Result<Vec<VirtualFolder>> {
        let Some(user) = &self.user else {
            return Ok(Vec::new());
        };
        let views: ItemsResult = self.get(&format!("/Users/{user}/Views"), &[]).await?;
        Ok(views
            .items
            .into_iter()
            .map(|v| VirtualFolder {
                name: v.name,
                item_id: v.id,
                collection_type: v.collection_type,
                locations: Vec::new(),
            })
            .collect())
    }

    pub fn has_user(&self) -> bool {
        self.user.is_some()
    }

    /// One page of movies and episodes below `library_id`.
    pub async fn items(
        &mut self,
        library_id: &str,
        start: usize,
        limit: usize,
    ) -> Result<ItemsResult> {
        let query = [
            ("ParentId", library_id.to_string()),
            ("Recursive", "true".into()),
            ("IncludeItemTypes", "Movie,Episode".into()),
            ("IsMissing", "false".into()),
            ("Fields", FIELDS.into()),
            ("SortBy", "SortName".into()),
            ("StartIndex", start.to_string()),
            ("Limit", limit.to_string()),
        ];
        if let Some(user) = &self.legacy_user {
            return self.get(&format!("/Users/{user}/Items"), &query).await;
        }
        match self.get("/Items", &query).await {
            Err(e) if matches!(e.status, Some(400 | 404)) => {
                // Servers before 10.9 need a user for item queries.
                let user = match &self.user {
                    Some(u) => u.clone(),
                    None => self.admin_user().await?,
                };
                self.legacy_user = Some(user.clone());
                self.get(&format!("/Users/{user}/Items"), &query).await
            }
            other => other,
        }
    }

    async fn admin_user(&self) -> Result<String> {
        let users: Vec<User> = self.get("/Users", &[]).await?;
        users
            .iter()
            .find(|u| u.policy.as_ref().is_some_and(|p| p.is_administrator))
            .or(users.first())
            .and_then(|u| u.id.clone())
            .ok_or_else(|| ClientError::new(tr!("The server has no user to read the library with")))
    }

    // ------------------------------------------------------- Quick Connect

    /// Start a Quick Connect request; the user confirms its code in Jellyfin.
    pub async fn quick_connect_initiate(&self) -> Result<QuickConnectResult> {
        let enabled: bool = self.get("/QuickConnect/Enabled", &[]).await?;
        if !enabled {
            return Err(ClientError::new(tr!(
                "Quick Connect is disabled on the server (Dashboard → General → Quick Connect)"
            )));
        }
        // POST since 10.9, GET before.
        let result = match self
            .send::<QuickConnectResult>(
                self.http
                    .post(format!("{}/QuickConnect/Initiate", self.base))
                    // Proxies (nginx, HTTP/2) reject a POST without body.
                    .json(&serde_json::json!({})),
            )
            .await
        {
            Err(e) if matches!(e.status, Some(404 | 405)) => {
                self.get("/QuickConnect/Initiate", &[]).await
            }
            other => other,
        }?;
        if result.secret.is_none() || result.code.is_none() {
            return Err(ClientError::new(tr!(
                "The server did not return a Quick Connect code"
            )));
        }
        Ok(result)
    }

    /// Whether the code was confirmed yet.
    pub async fn quick_connect_state(&self, secret: &str) -> Result<bool> {
        let r: QuickConnectResult = self
            .get("/QuickConnect/Connect", &[("Secret", secret.to_string())])
            .await
            .map_err(|e| {
                if e.status == Some(404) {
                    ClientError::new(tr!("The Quick Connect code expired – please start again"))
                } else {
                    e
                }
            })?;
        Ok(r.authenticated)
    }

    /// Exchange a confirmed request for an access token.
    pub async fn quick_connect_login(&self, secret: &str) -> Result<AuthenticationResult> {
        self.send(
            self.http
                .post(format!("{}/Users/AuthenticateWithQuickConnect", self.base))
                .json(&serde_json::json!({ "Secret": secret })),
        )
        .await
    }

    async fn send<T: DeserializeOwned>(&self, request: reqwest::RequestBuilder) -> Result<T> {
        let response = request
            .header(reqwest::header::AUTHORIZATION, &self.auth)
            .send()
            .await
            .map_err(|e| request_error(&self.base, &e))?;
        // Without the query: it may carry the Quick Connect secret.
        let mut url = response.url().clone();
        url.set_query(None);
        let url = url.to_string();
        let status = response.status();
        if !status.is_success() {
            return Err(status_error(status, &url));
        }
        response
            .json::<T>()
            .await
            .map_err(|e| ClientError::new(trf!("Unexpected answer from {}: {}", url, e)))
    }
}

fn request_error(base: &str, e: &reqwest::Error) -> ClientError {
    if e.is_timeout() {
        ClientError::new(trf!("{} did not answer in time", base))
    } else if e.is_connect() {
        ClientError::new(trf!("Cannot connect to {}: {}", base, root_cause(e)))
    } else {
        ClientError::new(trf!("Request to {} failed: {}", base, root_cause(e)))
    }
}

/// The innermost error says what really went wrong (DNS, TLS, refused …).
fn root_cause(e: &(dyn std::error::Error + 'static)) -> String {
    let mut cur = e;
    while let Some(next) = cur.source() {
        cur = next;
    }
    cur.to_string()
}

fn status_error(status: StatusCode, url: &str) -> ClientError {
    let code = status.as_u16();
    let message = match status {
        StatusCode::UNAUTHORIZED => {
            tr!("The API key or login was rejected (401) – check the key or log in again").into()
        }
        StatusCode::FORBIDDEN => tr!("Access denied (403) – the user may not read this").into(),
        _ => trf!("{} answered with an error ({})", url, code),
    };
    ClientError {
        message,
        status: Some(code),
    }
}
