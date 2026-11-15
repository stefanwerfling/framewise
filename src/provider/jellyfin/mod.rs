//! Jellyfin as library source: lists movies and episodes through the API and
//! maps their stream information to
//! [`MediaFile`](crate::media::model::MediaFile)s.

pub mod client;
pub mod dto;
pub mod mapping;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

pub use client::{Client, ClientError};
use mapping::LibraryContext;

use crate::core::config::JellyfinSettings;
use crate::scanner::{ScanIssue, ScanOutcome, ScanProgress};
use crate::trf;

/// A library on the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Library {
    pub id: String,
    pub name: String,
    /// `movies`, `tvshows`, `mixed` … (`None` = mixed content).
    pub collection_type: Option<String>,
    pub locations: Vec<String>,
}

impl Library {
    /// Libraries read when none were chosen: everything that can hold
    /// movies or series.
    pub fn is_video(&self) -> bool {
        matches!(
            self.collection_type.as_deref(),
            None | Some("movies" | "tvshows" | "mixed" | "homevideos")
        )
    }
}

/// Result of a connection test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    pub id: String,
    pub libraries: Vec<Library>,
}

fn client(settings: &JellyfinSettings) -> Result<Client, ClientError> {
    Ok(
        Client::new(&settings.url, &settings.api_key, &settings.device_id)?
            .with_user(&settings.user_id),
    )
}

pub async fn connect(settings: &JellyfinSettings) -> Result<ServerInfo, ClientError> {
    let client = client(settings)?;
    let info = client.system_info().await?;
    let folders = match client.libraries().await {
        // A Quick Connect user without administrator rights sees only its
        // own library views (without folders).
        Err(e) if client.has_user() && matches!(e.status, Some(401 | 403)) => {
            client.user_views().await?
        }
        other => other?,
    };
    let libraries = folders
        .into_iter()
        .filter_map(|f| {
            Some(Library {
                id: f.item_id?,
                name: f.name.unwrap_or_default(),
                collection_type: f.collection_type,
                locations: f.locations,
            })
        })
        .collect();
    Ok(ServerInfo {
        name: info.server_name.unwrap_or_default(),
        version: info.version.unwrap_or_default(),
        id: info.id.unwrap_or_default(),
        libraries,
    })
}

/// The libraries to read according to the settings.
pub fn selected<'a>(settings: &JellyfinSettings, all: &'a [Library]) -> Vec<&'a Library> {
    if settings.libraries.is_empty() {
        all.iter().filter(|l| l.is_video()).collect()
    } else {
        all.iter()
            .filter(|l| settings.libraries.contains(&l.id))
            .collect()
    }
}

/// Read all movies and episodes of the selected libraries.
///
/// Fails only if the server cannot be used at all; a library that cannot be
/// read is reported as issue and the others are still listed.
pub async fn scan<F>(
    settings: &JellyfinSettings,
    cancel: Arc<AtomicBool>,
    progress: F,
) -> Result<ScanOutcome, ClientError>
where
    F: Fn(ScanProgress),
{
    progress(ScanProgress::Discovering);
    let server = connect(settings).await?;
    let mut client = client(settings)?;
    let libraries = selected(settings, &server.libraries);

    let mut outcome = ScanOutcome::default();
    // Totals first, so the progress bar is meaningful.
    let mut totals = Vec::with_capacity(libraries.len());
    for lib in &libraries {
        match client.items(&lib.id, 0, 0).await {
            Ok(r) => totals.push(r.total_record_count.unwrap_or(0)),
            Err(e) => {
                outcome.issues.push(issue(lib, &e));
                totals.push(0);
            }
        }
    }
    let total: usize = totals.iter().sum();
    progress(ScanProgress::Discovered { total });

    let mut done = 0;
    for (lib, lib_total) in libraries.iter().zip(totals) {
        let context = LibraryContext {
            name: lib.name.clone(),
            locations: lib.locations.clone(),
        };
        let mut start = 0;
        while start < lib_total {
            if cancel.load(Ordering::Relaxed) {
                outcome.cancelled = true;
                return Ok(outcome);
            }
            let page = match client.items(&lib.id, start, client::PAGE_SIZE).await {
                Ok(p) => p,
                Err(e) => {
                    outcome.issues.push(issue(lib, &e));
                    break;
                }
            };
            if page.items.is_empty() {
                break;
            }
            start += page.items.len();
            done += page.items.len();
            let mut last = None;
            for item in &page.items {
                let files = mapping::map_item(item, &context);
                if let Some(f) = files.first() {
                    last = Some(f.relative_path.clone());
                }
                outcome.files.extend(files);
            }
            progress(ScanProgress::Probed {
                done: done.min(total),
                total,
                current: last.unwrap_or_else(|| PathBuf::from(&lib.name)),
            });
        }
    }
    outcome
        .files
        .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(outcome)
}

/// The original file of a server item, for ffprobe (range requests, no
/// conversion by the server).
pub fn stream_input(
    settings: &JellyfinSettings,
    info: &crate::provider::ProviderInfo,
) -> crate::health::check::Input {
    crate::health::check::Input::Http {
        url: format!(
            "{}/Videos/{}/stream?static=true&mediaSourceId={}",
            settings.url.trim().trim_end_matches('/'),
            info.item_id,
            info.media_source_id
        ),
        authorization: client::authorization(&settings.api_key, &settings.device_id),
    }
}

/// A new id for [`JellyfinSettings::device_id`].
pub fn new_device_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    h.write_u32(std::process::id());
    format!("framewise-{:016x}", h.finish())
}

/// Result of a Quick Connect login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Login {
    pub token: String,
    pub user_id: String,
    pub user_name: String,
}

/// Progress of [`quick_connect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuickConnectEvent {
    /// Enter this code in Jellyfin (user menu → Quick Connect).
    Code(String),
    /// Finished; `Err` also when cancelled or expired.
    Done(Result<Login, String>),
}

/// How long a code is waited for (Jellyfin drops requests after 10 min).
const QUICK_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Log in with Quick Connect: show a code, wait until the user confirms it
/// in Jellyfin, then fetch an access token bound to `device_id`.
pub async fn quick_connect<F>(
    url: &str,
    device_id: &str,
    poll: std::time::Duration,
    cancel: Arc<AtomicBool>,
    events: F,
) where
    F: Fn(QuickConnectEvent),
{
    let result = async {
        let client = Client::new(url, "", device_id)?;
        let start = client.quick_connect_initiate().await?;
        let secret = start.secret.unwrap_or_default();
        events(QuickConnectEvent::Code(start.code.unwrap_or_default()));
        let deadline = std::time::Instant::now() + QUICK_CONNECT_TIMEOUT;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(ClientError {
                    message: crate::tr!("Login cancelled").into(),
                    status: None,
                });
            }
            if std::time::Instant::now() > deadline {
                return Err(ClientError {
                    message: crate::tr!("The Quick Connect code expired – please start again")
                        .into(),
                    status: None,
                });
            }
            if client.quick_connect_state(&secret).await? {
                break;
            }
            tokio::time::sleep(poll).await;
        }
        let auth = client.quick_connect_login(&secret).await?;
        let user = auth.user.unwrap_or_default();
        match (auth.access_token, user.id) {
            (Some(token), Some(user_id)) => Ok(Login {
                token,
                user_id,
                user_name: user.name.unwrap_or_default(),
            }),
            _ => Err(ClientError {
                message: crate::tr!("The server did not return an access token").into(),
                status: None,
            }),
        }
    }
    .await;
    events(QuickConnectEvent::Done(result.map_err(|e| e.message)));
}

fn issue(lib: &Library, e: &ClientError) -> ScanIssue {
    ScanIssue {
        path: PathBuf::from(&lib.name),
        message: trf!("Library {} could not be read: {}", lib.name, e),
    }
}

#[cfg(test)]
mod tests;
