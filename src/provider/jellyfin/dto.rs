//! Subset of the Jellyfin API types that framewise reads. Every field is
//! optional: servers of different versions omit different fields, and some
//! send `null` where others leave a field out.

use serde::{Deserialize, Deserializer};

/// `null` → default (serde's `default` only covers missing fields).
fn nullable<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct SystemInfo {
    pub server_name: Option<String>,
    pub version: Option<String>,
    pub id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct VirtualFolder {
    pub name: Option<String>,
    pub item_id: Option<String>,
    pub collection_type: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub locations: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct UserPolicy {
    #[serde(deserialize_with = "nullable")]
    pub is_administrator: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct User {
    pub id: Option<String>,
    pub name: Option<String>,
    pub policy: Option<UserPolicy>,
}

/// Answer of `/QuickConnect/Initiate` and `/QuickConnect/Connect`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct QuickConnectResult {
    #[serde(deserialize_with = "nullable")]
    pub authenticated: bool,
    pub secret: Option<String>,
    pub code: Option<String>,
}

/// Answer of `/Users/AuthenticateWithQuickConnect`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct AuthenticationResult {
    pub user: Option<User>,
    pub access_token: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct ItemsResult {
    #[serde(deserialize_with = "nullable")]
    pub items: Vec<BaseItem>,
    pub total_record_count: Option<usize>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct BaseItem {
    pub id: Option<String>,
    /// Only set on library views (`/Users/{id}/Views`).
    pub collection_type: Option<String>,
    pub server_id: Option<String>,
    pub name: Option<String>,
    #[serde(rename = "Type")]
    pub item_type: Option<String>,
    pub path: Option<String>,
    pub production_year: Option<i32>,
    pub series_name: Option<String>,
    pub parent_index_number: Option<u32>,
    pub index_number: Option<u32>,
    pub location_type: Option<String>,
    pub date_created: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub media_sources: Vec<MediaSource>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct MediaSource {
    pub id: Option<String>,
    pub path: Option<String>,
    pub container: Option<String>,
    pub size: Option<u64>,
    pub bitrate: Option<u64>,
    pub run_time_ticks: Option<u64>,
    #[serde(deserialize_with = "nullable")]
    pub media_streams: Vec<MediaStream>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct MediaStream {
    #[serde(rename = "Type")]
    pub stream_type: Option<String>,
    pub index: Option<u32>,
    pub codec: Option<String>,
    pub codec_tag: Option<String>,
    pub profile: Option<String>,
    pub language: Option<String>,
    pub title: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub is_default: bool,
    #[serde(deserialize_with = "nullable")]
    pub is_forced: bool,
    #[serde(deserialize_with = "nullable")]
    pub is_hearing_impaired: bool,
    #[serde(deserialize_with = "nullable")]
    pub is_external: bool,
    pub is_interlaced: Option<bool>,
    // video
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub bit_rate: Option<u64>,
    pub bit_depth: Option<u8>,
    pub pixel_format: Option<String>,
    pub aspect_ratio: Option<String>,
    pub average_frame_rate: Option<f64>,
    pub real_frame_rate: Option<f64>,
    pub color_range: Option<String>,
    pub color_space: Option<String>,
    pub color_transfer: Option<String>,
    pub color_primaries: Option<String>,
    pub video_range_type: Option<String>,
    pub dv_profile: Option<u8>,
    pub dv_level: Option<u8>,
    pub el_present_flag: Option<u8>,
    pub dv_bl_signal_compatibility_id: Option<u8>,
    pub hdr10_plus_present_flag: Option<bool>,
    // audio
    pub channels: Option<u32>,
    pub channel_layout: Option<String>,
    pub sample_rate: Option<u32>,
}
