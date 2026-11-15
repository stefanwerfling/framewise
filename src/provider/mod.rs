//! External library providers (media servers).
//!
//! A provider lists the items of a server through its API and maps them to
//! [`MediaFile`](crate::media::model::MediaFile)s, so the normal analysis can
//! rate them against the profile. No file is read: the stream information is
//! what the server determined, and the result is a report only.

pub mod jellyfin;
pub mod summary;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProviderKind {
    Jellyfin,
}

impl ProviderKind {
    pub fn label(self) -> &'static str {
        match self {
            ProviderKind::Jellyfin => "Jellyfin",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ItemKind {
    Movie,
    Episode,
}

/// Where a provider item came from and what it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub provider: ProviderKind,
    pub server_id: String,
    pub item_id: String,
    /// One item can have several versions (files); each is one media source.
    pub media_source_id: String,
    pub kind: ItemKind,
    pub title: String,
    pub year: Option<i32>,
    pub series: Option<String>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub library: String,
    /// Path of the file on the server (not accessible from here).
    pub server_path: String,
    /// The server has no stream information for this file (not analysed yet).
    pub missing_streams: bool,
}

impl ProviderInfo {
    /// `Title (2001)` or `Series – S01E02 – Title`.
    pub fn display_title(&self) -> String {
        match self.kind {
            ItemKind::Movie => match self.year {
                Some(y) => format!("{} ({y})", self.title),
                None => self.title.clone(),
            },
            ItemKind::Episode => {
                let code = match (self.season, self.episode) {
                    (Some(s), Some(e)) => format!("S{s:02}E{e:02}"),
                    (Some(s), None) => format!("S{s:02}"),
                    (None, Some(e)) => format!("E{e:02}"),
                    (None, None) => String::new(),
                };
                let series = self.series.as_deref().unwrap_or_default();
                [series, code.as_str(), self.title.as_str()]
                    .into_iter()
                    .filter(|p| !p.is_empty())
                    .collect::<Vec<_>>()
                    .join(" – ")
            }
        }
    }
}
