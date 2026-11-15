//! Minimal Jellyfin API client (read only).

use std::time::Duration;

use reqwest::StatusCode;
use serde::de::DeserializeOwned;

use super::dto::{ItemsResult, SystemInfo, User, VirtualFolder};
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
    /// Set once `/Items` was rejected without a user (servers before 10.9).
    user_id: Option<String>,
}

impl Client {
    pub fn new(url: &str, api_key: &str) -> Result<Self> {
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
        let auth = format!(
            "MediaBrowser Client=\"framewise\", Device=\"framewise\", DeviceId=\"framewise\", Version=\"{}\", Token=\"{}\"",
            env!("CARGO_PKG_VERSION"),
            api_key.trim()
        );
        Ok(Self {
            http,
            base,
            auth,
            user_id: None,
        })
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
        let url = format!("{}{path}", self.base);
        let response = self
            .http
            .get(&url)
            .header(reqwest::header::AUTHORIZATION, &self.auth)
            .query(query)
            .send()
            .await
            .map_err(|e| request_error(&self.base, &e))?;
        let status = response.status();
        if !status.is_success() {
            return Err(status_error(status, &url));
        }
        response
            .json::<T>()
            .await
            .map_err(|e| ClientError::new(trf!("Unexpected answer from {}: {}", url, e)))
    }

    pub async fn system_info(&self) -> Result<SystemInfo> {
        self.get("/System/Info", &[]).await
    }

    pub async fn libraries(&self) -> Result<Vec<VirtualFolder>> {
        self.get("/Library/VirtualFolders", &[]).await
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
        if let Some(user) = &self.user_id {
            return self.get(&format!("/Users/{user}/Items"), &query).await;
        }
        match self.get("/Items", &query).await {
            Err(e) if matches!(e.status, Some(400 | 404)) => {
                // Servers before 10.9 need a user for item queries.
                let user = self.admin_user().await?;
                self.user_id = Some(user.clone());
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
        StatusCode::UNAUTHORIZED => tr!("The API key was rejected (401)").into(),
        StatusCode::FORBIDDEN => {
            tr!("The API key may not read this (403) – use a key from Dashboard → API keys").into()
        }
        _ => trf!("{} answered with an error ({})", url, code),
    };
    ClientError {
        message,
        status: Some(code),
    }
}
