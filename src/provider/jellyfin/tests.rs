use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::dto::ItemsResult;
use super::mapping::{self, LibraryContext, frame_rate, parse_iso_date};
use super::*;
use crate::analyzer::recommend::{self, Recommendation};
use crate::encoding::plan::{PlanBlocker, PlanCapabilities};
use crate::encoding::profile::EncodingProfile;
use crate::media::model::{
    AudioCodec, DvCompatibility, HdrFormat, Ratio, SubtitleCodec, VideoCodec,
};
use crate::provider::ItemKind;

/// Abridged answer of Jellyfin 10.10 (`/Items?Fields=MediaSources,…`).
const ITEMS: &str = r#"{
  "Items": [
    {
      "Name": "Dune: Part Two", "ServerId": "srv", "Id": "m1", "Type": "Movie",
      "ProductionYear": 2024, "LocationType": "FileSystem",
      "Path": "/media/movies/Dune Part Two (2024)/Dune Part Two (2024).mkv",
      "DateCreated": "2024-06-01T12:00:00.0000000Z",
      "MediaSources": [{
        "Id": "m1", "Container": "mkv", "Size": 64000000000, "Bitrate": 61000000,
        "RunTimeTicks": 99000000000,
        "Path": "/media/movies/Dune Part Two (2024)/Dune Part Two (2024).mkv",
        "MediaStreams": [
          {"Type": "Video", "Index": 0, "Codec": "hevc", "Profile": "Main 10",
           "Width": 3840, "Height": 2160, "BitDepth": 10, "PixelFormat": "yuv420p10le",
           "AspectRatio": "16:9", "RealFrameRate": 23.976025, "IsInterlaced": false,
           "ColorTransfer": "smpte2084", "ColorPrimaries": "bt2020", "ColorSpace": "bt2020nc",
           "VideoRange": "HDR", "VideoRangeType": "DOVIWithHDR10",
           "DvProfile": 8, "DvLevel": 6, "ElPresentFlag": 0, "DvBlSignalCompatibilityId": 1,
           "IsDefault": true},
          {"Type": "Audio", "Index": 1, "Codec": "truehd", "Profile": "Dolby TrueHD + Dolby Atmos",
           "Language": "eng", "Channels": 8, "ChannelLayout": "7.1", "SampleRate": 48000,
           "Title": "English Atmos", "IsDefault": true},
          {"Type": "Audio", "Index": 2, "Codec": "dts", "Profile": "DTS-HD MA",
           "Language": "ger", "Channels": 6, "ChannelLayout": "5.1(side)", "IsDefault": false},
          {"Type": "Subtitle", "Index": 3, "Codec": "PGSSUB", "Language": "ger",
           "IsForced": true, "IsDefault": false},
          {"Type": "Subtitle", "Index": 4, "Codec": "subrip", "Language": "eng",
           "IsExternal": true, "Path": "/media/movies/Dune Part Two (2024)/Dune.eng.srt"},
          {"Type": "EmbeddedImage", "Index": 5, "Codec": "mjpeg", "Width": 600, "Height": 900}
        ]
      }]
    },
    {
      "Name": "Pilot", "ServerId": "srv", "Id": "e1", "Type": "Episode",
      "SeriesName": "The Show", "ParentIndexNumber": 1, "IndexNumber": 1,
      "LocationType": "FileSystem", "Path": "/media/tv/The Show/Season 01/The Show S01E01.mkv",
      "MediaSources": [{
        "Id": "e1", "Container": "mkv", "Size": 1200000000, "RunTimeTicks": 26000000000,
        "MediaStreams": [
          {"Type": "Video", "Index": 0, "Codec": "h264", "Profile": "High",
           "Width": 1920, "Height": 1080, "BitDepth": 8, "PixelFormat": "yuv420p",
           "RealFrameRate": 25, "VideoRangeType": "SDR", "IsDefault": true},
          {"Type": "Audio", "Index": 1, "Codec": "ac3", "Language": "ger", "Channels": 6,
           "IsDefault": true},
          {"Type": "Subtitle", "Index": 2, "Codec": "subrip", "Language": "ger",
           "Title": "Forced", "IsForced": null, "IsDefault": null}
        ]
      }]
    },
    {
      "Name": "Not analysed yet", "ServerId": "srv", "Id": "e2", "Type": "Episode",
      "SeriesName": "The Show", "ParentIndexNumber": 1, "IndexNumber": 2,
      "LocationType": "FileSystem", "Path": "/media/tv/The Show/Season 01/The Show S01E02.mkv",
      "MediaSources": [{"Id": "e2", "Container": "mkv", "MediaStreams": null}]
    },
    {
      "Name": "Missing", "ServerId": "srv", "Id": "e3", "Type": "Episode",
      "SeriesName": "The Show", "ParentIndexNumber": 1, "IndexNumber": 3,
      "LocationType": "Virtual", "MediaSources": null
    },
    {
      "Name": "Two Versions", "ServerId": "srv", "Id": "m2", "Type": "Movie",
      "LocationType": "FileSystem", "Path": "/media/movies/Two/Two - 1080p.mp4",
      "MediaSources": [
        {"Id": "m2a", "Container": "mp4", "Path": "/media/movies/Two/Two - 1080p.mp4",
         "MediaStreams": [{"Type": "Video", "Index": 0, "Codec": "h264", "Width": 1920, "Height": 1080}]},
        {"Id": "m2b", "Container": "mkv", "Path": "/media/movies/Two/Two - 2160p.mkv",
         "MediaStreams": [{"Type": "Video", "Index": 0, "Codec": "hevc", "Width": 3840, "Height": 2160}]}
      ]
    },
    {
      "Name": "Profile Five", "ServerId": "srv", "Id": "m3", "Type": "Movie",
      "LocationType": "FileSystem", "Path": "/media/movies/P5.mp4",
      "MediaSources": [{"Id": "m3", "Container": "mp4", "MediaStreams": [
        {"Type": "Video", "Index": 0, "Codec": "hevc", "Width": 3840, "Height": 2160,
         "BitDepth": 10, "CodecTag": "dvh1", "VideoRangeType": "DOVI", "DvProfile": 5,
         "DvBlSignalCompatibilityId": 0}
      ]}]
    },
    {
      "Name": "Old Server HLG", "ServerId": "srv", "Id": "m4", "Type": "Movie",
      "LocationType": "FileSystem", "Path": "/media/movies/HLG.mkv",
      "MediaSources": [{"Id": "m4", "Container": "mkv", "MediaStreams": [
        {"Type": "Video", "Index": 0, "Codec": "hevc", "Width": 3840, "Height": 2160,
         "VideoRangeType": "HLG"}
      ]}]
    },
    {"Name": "A Season", "Id": "s1", "Type": "Season"}
  ],
  "TotalRecordCount": 8,
  "StartIndex": 0
}"#;

fn mapped() -> Vec<crate::media::model::MediaFile> {
    let result: ItemsResult = serde_json::from_str(ITEMS).expect("parse");
    let movies = LibraryContext {
        name: "Movies".into(),
        locations: vec!["/media/movies".into()],
    };
    result
        .items
        .iter()
        .flat_map(|i| mapping::map_item(i, &movies))
        .collect()
}

fn find<'a>(
    files: &'a [crate::media::model::MediaFile],
    id: &str,
) -> &'a crate::media::model::MediaFile {
    files
        .iter()
        .find(|f| f.provider.as_ref().unwrap().media_source_id == id)
        .unwrap_or_else(|| panic!("{id} not mapped"))
}

#[test]
fn maps_movies_episodes_and_versions() {
    let files = mapped();
    let ids: Vec<&str> = files
        .iter()
        .map(|f| f.provider.as_ref().unwrap().media_source_id.as_str())
        .collect();
    // Virtual episode and the season are skipped; two versions → two files.
    assert_eq!(ids, ["m1", "e1", "e2", "m2a", "m2b", "m3", "m4"]);

    let dune = find(&files, "m1");
    let p = dune.provider.as_ref().unwrap();
    assert_eq!(p.kind, ItemKind::Movie);
    assert_eq!(p.display_title(), "Dune: Part Two (2024)");
    assert_eq!(dune.absolute_path, PathBuf::from("jellyfin://srv/m1"));
    assert_eq!(
        dune.relative_path,
        PathBuf::from("Dune Part Two (2024)/Dune Part Two (2024).mkv"),
        "relative to the library folder"
    );
    assert_eq!(dune.extension, "mkv");
    assert_eq!(dune.size_bytes, 64_000_000_000);
    assert_eq!(dune.duration_s(), Some(9900.0));
    assert_eq!(dune.modified_unix, 1_717_243_200);
    assert!(!dune.is_local());

    let ep = find(&files, "e1");
    let p = ep.provider.as_ref().unwrap();
    assert_eq!(p.display_title(), "The Show – S01E01 – Pilot");
    // Not below the movie library folder → series/season fallback.
    assert_eq!(
        ep.relative_path,
        PathBuf::from("The Show/Season 1/The Show S01E01.mkv")
    );
}

#[test]
fn maps_streams_like_ffprobe() {
    let files = mapped();
    let dune = find(&files, "m1");
    let v = dune.primary_video().unwrap();
    assert_eq!(v.codec, VideoCodec::Hevc);
    assert_eq!((v.width, v.height, v.bit_depth), (3840, 2160, Some(10)));
    assert_eq!(v.frame_rate, Ratio::new(24000, 1001));
    assert_eq!(v.display_aspect_ratio, Ratio::new(16, 9));
    match &v.hdr {
        HdrFormat::DolbyVision(dv) => {
            assert_eq!(dv.profile, Some(8));
            assert_eq!(dv.compatibility, DvCompatibility::Hdr10);
            assert!(!dv.has_enhancement_layer);
        }
        other => panic!("expected Dolby Vision, got {other:?}"),
    }
    // Cover art is kept as attached picture, like ffprobe reports it.
    assert_eq!(dune.video.len(), 2);
    assert!(dune.video[1].attached_pic);

    assert_eq!(dune.audio.len(), 2);
    assert_eq!(dune.audio[0].codec, AudioCodec::TrueHd);
    assert!(dune.audio[0].atmos.is_present());
    assert_eq!(dune.audio[1].codec, AudioCodec::DtsHdMa);
    assert_eq!(dune.audio[1].language.as_deref(), Some("ger"));

    // External subtitle files are not part of the container.
    assert_eq!(dune.subtitles.len(), 1);
    assert_eq!(dune.subtitles[0].codec, SubtitleCodec::Pgs);
    assert!(dune.subtitles[0].forced);

    let ep = find(&files, "e1");
    let s = &ep.subtitles[0];
    assert!(s.forced, "forced from the title, null flags tolerated");
    assert!(!s.default);
    assert_eq!(ep.primary_video().unwrap().hdr, HdrFormat::Sdr);
    assert_eq!(ep.primary_video().unwrap().frame_rate, Ratio::new(25, 1));

    let hlg = find(&files, "m4");
    assert_eq!(
        hlg.primary_video().unwrap().hdr,
        HdrFormat::Hlg,
        "range type fills a missing transfer"
    );
}

#[test]
fn ratings_follow_the_profile() {
    let files = mapped();
    let profile = EncodingProfile::universal();
    let caps = PlanCapabilities::default();
    let rec = |id: &str| recommend::analyze(find(&files, id), &profile, &caps);

    // Not analysed by the server: nothing to judge → review.
    let missing = find(&files, "e2");
    assert!(missing.provider.as_ref().unwrap().missing_streams);
    let a = rec("e2");
    assert_eq!(a.recommendation, Recommendation::Review);
    assert!(a.plan.blockers.contains(&PlanBlocker::NoVideo));

    // Profile 5 cannot be converted.
    assert_eq!(rec("m3").recommendation, Recommendation::Review);

    // UHD HEVC with Dolby Vision has to be re-encoded for the universal profile.
    assert_eq!(rec("m1").recommendation, Recommendation::Transcode);
    assert!(!rec("e1").plan.is_blocked());
}

#[test]
fn frame_rates_and_dates() {
    assert_eq!(frame_rate(23.976), Ratio::new(24000, 1001));
    assert_eq!(frame_rate(29.97003), Ratio::new(30000, 1001));
    assert_eq!(frame_rate(59.94), Ratio::new(60000, 1001));
    assert_eq!(frame_rate(24.0), Ratio::new(24, 1));
    assert_eq!(frame_rate(12.5), Ratio::new(25, 2));
    assert_eq!(frame_rate(0.0), None);
    assert_eq!(frame_rate(f64::NAN), None);
    assert_eq!(parse_iso_date("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(
        parse_iso_date("2000-03-01T01:02:03.1234567Z"),
        Some(951_872_523)
    );
    assert_eq!(parse_iso_date("garbage"), None);
}

#[test]
fn library_selection() {
    let lib = |id: &str, t: Option<&str>| Library {
        id: id.into(),
        name: id.into(),
        collection_type: t.map(Into::into),
        locations: vec![],
    };
    let all = [
        lib("m", Some("movies")),
        lib("t", Some("tvshows")),
        lib("x", None),
        lib("music", Some("music")),
        lib("books", Some("books")),
    ];
    let mut settings = JellyfinSettings::default();
    let ids = |s: &JellyfinSettings| -> Vec<String> {
        selected(s, &all)
            .into_iter()
            .map(|l| l.id.clone())
            .collect()
    };
    assert_eq!(ids(&settings), ["m", "t", "x"]);
    settings.libraries = vec!["t".into(), "gone".into()];
    assert_eq!(ids(&settings), ["t"]);
}

// ------------------------------------------------------------ HTTP stub

#[derive(Default)]
struct Stub {
    movies: usize,
    /// `/Items` without user answers 400 (servers before 10.9).
    legacy: bool,
    requests: Mutex<Vec<String>>,
    item_pages: AtomicUsize,
}

fn movie(i: usize) -> Value {
    json!({
        "Name": format!("Movie {i:03}"), "ServerId": "srv", "Id": format!("m{i}"),
        "Type": "Movie", "LocationType": "FileSystem",
        "Path": format!("/media/movies/Movie {i:03}.mkv"),
        "MediaSources": [{"Id": format!("m{i}"), "Container": "mkv", "Size": 1000,
            "MediaStreams": [{"Type": "Video", "Index": 0, "Codec": "h264",
                              "Width": 1280, "Height": 720}]}]
    })
}

fn query(target: &str) -> HashMap<String, String> {
    target
        .split_once('?')
        .map(|(_, q)| q)
        .unwrap_or_default()
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.replace("%2C", ",")))
        .collect()
}

impl Stub {
    fn answer(&self, target: &str, authorized: bool) -> (u16, Value) {
        if !authorized {
            return (401, json!({}));
        }
        let path = target.split('?').next().unwrap_or_default();
        let q = query(target);
        match path {
            "/System/Info" => (
                200,
                json!({"ServerName": "Stub", "Version": "10.10.7", "Id": "srv"}),
            ),
            "/Library/VirtualFolders" => (
                200,
                json!([
                    {"Name": "Movies", "ItemId": "lib-m", "CollectionType": "movies",
                     "Locations": ["/media/movies"]},
                    {"Name": "Music", "ItemId": "lib-music", "CollectionType": "music",
                     "Locations": ["/media/music"]}
                ]),
            ),
            "/Users" => (
                200,
                json!([
                    {"Id": "viewer", "Policy": {"IsAdministrator": false}},
                    {"Id": "admin", "Policy": {"IsAdministrator": true}}
                ]),
            ),
            "/Items" if self.legacy => (400, json!({})),
            "/Items" | "/Users/admin/Items" => {
                assert_eq!(q.get("ParentId").map(String::as_str), Some("lib-m"));
                assert_eq!(
                    q.get("IncludeItemTypes").map(String::as_str),
                    Some("Movie,Episode")
                );
                let start: usize = q["StartIndex"].parse().unwrap();
                let limit: usize = q["Limit"].parse().unwrap();
                if limit > 0 {
                    self.item_pages.fetch_add(1, Ordering::Relaxed);
                }
                let items: Vec<Value> = (start..(start + limit).min(self.movies))
                    .map(movie)
                    .collect();
                (
                    200,
                    json!({"Items": items, "TotalRecordCount": self.movies, "StartIndex": start}),
                )
            }
            _ => (404, json!({})),
        }
    }
}

/// Serve `stub` on a local port; returns the base URL.
async fn serve(stub: Arc<Stub>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let stub = stub.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).into_owned();
                let target = head
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                let authorized = head.lines().any(|l| {
                    l.to_ascii_lowercase().starts_with("authorization:")
                        && l.contains("Token=\"secret\"")
                });
                stub.requests.lock().unwrap().push(target.clone());
                let (status, body) = stub.answer(&target, authorized);
                let body = body.to_string();
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/")
}

fn settings(url: &str, key: &str) -> JellyfinSettings {
    JellyfinSettings {
        url: url.into(),
        api_key: key.into(),
        libraries: vec![],
    }
}

#[tokio::test]
async fn scan_pages_through_video_libraries() {
    let stub = Arc::new(Stub {
        movies: 450,
        ..Stub::default()
    });
    let url = serve(stub.clone()).await;
    let progress = Arc::new(Mutex::new(Vec::new()));
    let p = progress.clone();
    let outcome = scan(
        &settings(&url, "secret"),
        Arc::new(AtomicBool::new(false)),
        move |e| {
            p.lock().unwrap().push(e);
        },
    )
    .await
    .expect("scan");
    assert!(!outcome.cancelled);
    assert!(outcome.issues.is_empty(), "{:?}", outcome.issues);
    assert_eq!(outcome.files.len(), 450);
    assert_eq!(
        stub.item_pages.load(Ordering::Relaxed),
        3,
        "pages of {}",
        client::PAGE_SIZE
    );
    let requests = stub.requests.lock().unwrap();
    assert!(
        !requests.iter().any(|r| r.contains("lib-music")),
        "music library is not read"
    );
    let progress = progress.lock().unwrap();
    assert_eq!(progress[1], ScanProgress::Discovered { total: 450 });
    assert!(matches!(
        progress.last(),
        Some(ScanProgress::Probed {
            done: 450,
            total: 450,
            ..
        })
    ));
}

#[tokio::test]
async fn older_servers_are_read_through_the_admin_user() {
    let stub = Arc::new(Stub {
        movies: 3,
        legacy: true,
        ..Stub::default()
    });
    let url = serve(stub.clone()).await;
    let outcome = scan(
        &settings(&url, "secret"),
        Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .await
    .expect("scan");
    assert_eq!(outcome.files.len(), 3);
    assert!(
        stub.requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.starts_with("/Users/admin/Items"))
    );
}

#[tokio::test]
async fn wrong_key_and_unreachable_server_fail_clearly() {
    let stub = Arc::new(Stub::default());
    let url = serve(stub).await;
    let err = connect(&settings(&url, "wrong")).await.unwrap_err();
    assert_eq!(err.status, Some(401));

    let err = connect(&settings("jellyfin.local", "secret"))
        .await
        .unwrap_err();
    assert!(err.status.is_none());

    // Port 9 (discard) on localhost is closed in practice.
    let err = connect(&settings("http://127.0.0.1:9", "secret"))
        .await
        .unwrap_err();
    assert!(err.status.is_none());
}

#[tokio::test]
async fn cancel_stops_before_the_next_page() {
    let stub = Arc::new(Stub {
        movies: 50,
        ..Stub::default()
    });
    let url = serve(stub.clone()).await;
    let outcome = scan(
        &settings(&url, "secret"),
        Arc::new(AtomicBool::new(true)),
        |_| {},
    )
    .await
    .expect("scan");
    assert!(outcome.cancelled);
    assert!(outcome.files.is_empty());
    assert_eq!(stub.item_pages.load(Ordering::Relaxed), 0);
}
