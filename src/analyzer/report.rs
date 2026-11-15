//! Library report: why each file has to be processed, and a CSV export.

use std::fmt::Write as _;

use crate::analyzer::recommend::Analysis;
use crate::encoding::plan::{AudioAction, AudioPurpose, SubtitleAction};
use crate::health::Health;
use crate::media::model::MediaFile;
use crate::provider::ItemKind;
use crate::{tr, trf};

/// Short reasons for the recommendation, most important first.
pub fn reasons(media: &MediaFile, analysis: &Analysis) -> Vec<String> {
    let plan = &analysis.plan;
    let mut out: Vec<String> = plan.blockers.iter().map(|b| b.describe()).collect();
    if media.provider.as_ref().is_some_and(|p| p.missing_streams) {
        out.push(tr!("The server has no stream information yet").into());
    }
    if let Some(v) = &plan.video {
        out.extend(v.reasons.iter().map(|r| r.describe()));
    }
    for a in &plan.audio {
        match (&a.action, a.purpose) {
            (AudioAction::Transcode { codec, .. }, AudioPurpose::Converted) => {
                let source = media
                    .audio
                    .get(a.source_type_index as usize)
                    .map_or("?", |s| s.codec.label());
                out.push(trf!(
                    "audio {} {} → {}",
                    a.language.as_deref().unwrap_or("und"),
                    source,
                    codec.label()
                ));
            }
            (_, AudioPurpose::CompatibilityStereo) => {
                out.push(tr!("additional AAC stereo track").into());
            }
            _ => {}
        }
    }
    for s in &plan.subtitles {
        let lang = s.language.as_deref().unwrap_or("und");
        match &s.action {
            SubtitleAction::MovText if s.codec.label() != "mov_text" => {
                out.push(trf!("subtitle {} {} → mov_text", lang, s.codec.label()));
            }
            SubtitleAction::Sidecar { .. } => {
                out.push(trf!("subtitle {} as separate file", lang));
            }
            SubtitleAction::Skip => out.push(trf!("subtitle {} {} dropped", lang, s.codec.label())),
            SubtitleAction::MovText => {}
        }
    }
    if !media.extension.is_empty() && media.extension != "mp4" {
        out.push(trf!("container {} → MP4", media.extension.to_uppercase()));
    }
    out
}

/// CSV for spreadsheets: `;`-separated with UTF-8 BOM, so Excel and
/// LibreOffice open it with umlauts intact in German locales. Sizes are
/// plain byte counts so they sort and sum correctly.
pub fn to_csv<'a>(
    rows: impl IntoIterator<Item = (&'a MediaFile, &'a Analysis, &'a Health)>,
) -> String {
    let mut out = String::from("\u{feff}");
    let header = [
        tr!("Type"),
        tr!("Series"),
        tr!("Season"),
        tr!("Episode"),
        tr!("Title"),
        tr!("Year"),
        tr!("Library"),
        tr!("Path"),
        tr!("Recommendation"),
        tr!("Reasons"),
        "Video",
        tr!("Resolution"),
        "HDR",
        tr!("Audio"),
        tr!("Size (bytes)"),
        tr!("Estimated savings (bytes)"),
        tr!("Health"),
        tr!("Findings"),
    ];
    line(&mut out, header.iter().map(|h| h.to_string()));
    for (m, a, h) in rows {
        let v = m.primary_video();
        let p = m.provider.as_ref();
        let opt = |x: Option<String>| x.unwrap_or_default();
        let kind = match p.map(|p| p.kind) {
            Some(ItemKind::Movie) => tr!("Movie"),
            Some(ItemKind::Episode) => tr!("Episode"),
            None => tr!("File"),
        };
        let audio: Vec<String> = m
            .audio
            .iter()
            .map(|s| {
                format!(
                    "{} {} {}",
                    s.language.as_deref().unwrap_or("und"),
                    s.codec.label(),
                    s.channel_label()
                )
            })
            .collect();
        line(
            &mut out,
            [
                kind.to_string(),
                opt(p.and_then(|p| p.series.clone())),
                opt(p.and_then(|p| p.season).map(|s| s.to_string())),
                opt(p.and_then(|p| p.episode).map(|e| e.to_string())),
                p.map_or_else(|| m.file_name.clone(), |p| p.title.clone()),
                opt(p.and_then(|p| p.year).map(|y| y.to_string())),
                opt(p.map(|p| p.library.clone())),
                p.map_or_else(
                    || m.absolute_path.display().to_string(),
                    |p| p.server_path.clone(),
                ),
                a.recommendation.label().to_string(),
                reasons(m, a).join(", "),
                opt(v.map(|v| v.codec.label().to_string())),
                opt(v.map(|v| format!("{}x{}", v.width, v.height))),
                opt(v.map(|v| v.hdr.label())),
                audio.join(" | "),
                m.size_bytes.to_string(),
                opt(a.estimated_savings.map(|s| s.to_string())),
                h.state().label().to_string(),
                h.findings()
                    .iter()
                    .map(|f| f.describe())
                    .collect::<Vec<_>>()
                    .join(", "),
            ],
        );
    }
    out
}

fn line(out: &mut String, fields: impl IntoIterator<Item = String>) {
    let mut first = true;
    for f in fields {
        if !first {
            out.push(';');
        }
        first = false;
        if f.contains([';', '"', '\n', '\r']) {
            let _ = write!(out, "\"{}\"", f.replace('"', "\"\""));
        } else {
            out.push_str(&f);
        }
    }
    out.push_str("\r\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::recommend;
    use crate::encoding::plan::PlanCapabilities;
    use crate::encoding::profile::EncodingProfile;
    use crate::media::fixtures::*;
    use crate::provider::{ProviderInfo, ProviderKind};

    #[test]
    fn csv_quotes_and_lists_reasons() {
        let mut local = media(
            "mkv",
            1000,
            10.0,
            vec![video("mpeg2video", 720, 576)],
            vec![],
            vec![],
        );
        local.file_name = "A; \"quoted\" name.mkv".into();
        let mut episode = local.clone();
        episode.provider = Some(ProviderInfo {
            provider: ProviderKind::Jellyfin,
            server_id: "s".into(),
            item_id: "i".into(),
            media_source_id: "i".into(),
            kind: ItemKind::Episode,
            title: "Pilot".into(),
            year: None,
            series: Some("Show".into()),
            season: Some(1),
            episode: Some(2),
            library: "TV".into(),
            server_path: "/tv/Show/S01E02.mkv".into(),
            missing_streams: false,
        });
        let profile = EncodingProfile::universal();
        let caps = PlanCapabilities::default();
        let a1 = recommend::analyze(&local, &profile, &caps);
        let a2 = recommend::analyze(&episode, &profile, &caps);

        let why = reasons(&local, &a1);
        assert!(why.iter().any(|r| r.contains("MPEG-2")), "{why:?}");
        assert!(why.iter().any(|r| r.contains("MKV")), "{why:?}");

        let healthy = Health::default();
        let broken = Health {
            risks: vec![crate::health::Finding::Interlaced],
            check: None,
        };
        let csv = to_csv([(&local, &a1, &healthy), (&episode, &a2, &broken)]);
        assert!(csv.starts_with('\u{feff}'));
        let lines: Vec<&str> = csv.trim_start_matches('\u{feff}').split("\r\n").collect();
        assert_eq!(lines.len(), 4, "header, two rows, trailing empty");
        assert_eq!(lines[0].split(';').count(), 18);
        assert!(lines[2].contains("interlaced"), "{}", lines[2]);
        assert!(
            lines[1].contains("\"A; \"\"quoted\"\" name.mkv\""),
            "{}",
            lines[1]
        );
        assert!(
            lines[2].contains(";Show;1;2;Pilot;;TV;/tv/Show/S01E02.mkv;"),
            "{}",
            lines[2]
        );
    }
}
