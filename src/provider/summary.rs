//! Overview of a provider library: movies and series → seasons with the
//! number of items per recommendation.

use std::collections::BTreeMap;

use crate::analyzer::recommend::Recommendation;
use crate::provider::{ItemKind, ProviderInfo};

/// Items per recommendation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub total: usize,
    pub transcode: usize,
    pub remux: usize,
    pub optimal: usize,
    pub review: usize,
}

impl Counts {
    fn add(&mut self, r: Recommendation) {
        self.total += 1;
        match r {
            Recommendation::Transcode => self.transcode += 1,
            Recommendation::Remux => self.remux += 1,
            Recommendation::AlreadyOptimal => self.optimal += 1,
            Recommendation::Review => self.review += 1,
        }
    }

    /// Items that have to be processed again (re-encode or remux).
    pub fn actionable(&self) -> usize {
        self.transcode + self.remux
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeasonSummary {
    /// `None` = episodes without season number (specials, unsorted).
    pub season: Option<u32>,
    pub counts: Counts,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesSummary {
    pub name: String,
    pub counts: Counts,
    /// Sorted by season number; `None` last.
    pub seasons: Vec<SeasonSummary>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibrarySummary {
    pub movies: Counts,
    pub episodes: Counts,
    /// Sorted by name (case-insensitive).
    pub series: Vec<SeriesSummary>,
}

pub fn summarize<'a>(
    items: impl IntoIterator<Item = (&'a ProviderInfo, Recommendation)>,
) -> LibrarySummary {
    let mut summary = LibrarySummary::default();
    let mut series: BTreeMap<(String, String), BTreeMap<(bool, u32), Counts>> = BTreeMap::new();
    for (info, rec) in items {
        match info.kind {
            ItemKind::Movie => summary.movies.add(rec),
            ItemKind::Episode => {
                summary.episodes.add(rec);
                let name = info.series.clone().unwrap_or_default();
                // (no season, number): seasons first, unnumbered last.
                let key = (info.season.is_none(), info.season.unwrap_or(0));
                series
                    .entry((name.to_lowercase(), name))
                    .or_default()
                    .entry(key)
                    .or_default()
                    .add(rec);
            }
        }
    }
    summary.series = series
        .into_iter()
        .map(|((_, name), seasons)| {
            let seasons: Vec<SeasonSummary> = seasons
                .into_iter()
                .map(|((none, n), counts)| SeasonSummary {
                    season: (!none).then_some(n),
                    counts,
                })
                .collect();
            let counts = seasons.iter().fold(Counts::default(), |mut acc, s| {
                acc.total += s.counts.total;
                acc.transcode += s.counts.transcode;
                acc.remux += s.counts.remux;
                acc.optimal += s.counts.optimal;
                acc.review += s.counts.review;
                acc
            });
            SeriesSummary {
                name,
                counts,
                seasons,
            }
        })
        .collect();
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderKind;

    fn ep(series: &str, season: Option<u32>, episode: u32) -> ProviderInfo {
        ProviderInfo {
            provider: ProviderKind::Jellyfin,
            server_id: "s".into(),
            item_id: format!("{series}{episode}"),
            media_source_id: format!("{series}{episode}"),
            kind: ItemKind::Episode,
            title: format!("Episode {episode}"),
            year: None,
            series: Some(series.into()),
            season,
            episode: Some(episode),
            library: "Series".into(),
            server_path: format!("/tv/{series}/{episode}.mkv"),
            missing_streams: false,
        }
    }

    #[test]
    fn groups_series_and_seasons() {
        let mut movie = ep("", None, 0);
        movie.kind = ItemKind::Movie;
        movie.series = None;
        let items = [
            (ep("b-Series", Some(2), 1), Recommendation::Transcode),
            (ep("b-Series", Some(1), 1), Recommendation::AlreadyOptimal),
            (ep("b-Series", Some(1), 2), Recommendation::Remux),
            (ep("b-Series", None, 9), Recommendation::Review),
            (ep("A-Series", Some(1), 1), Recommendation::Transcode),
            (movie, Recommendation::Transcode),
        ];
        let s = summarize(items.iter().map(|(i, r)| (i, *r)));
        assert_eq!(s.movies.total, 1);
        assert_eq!(s.episodes.total, 5);
        assert_eq!(s.episodes.actionable(), 3);
        let names: Vec<&str> = s.series.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, ["A-Series", "b-Series"], "case-insensitive order");
        let b = &s.series[1];
        assert_eq!(b.counts.total, 4);
        assert_eq!(b.counts.actionable(), 2);
        let seasons: Vec<Option<u32>> = b.seasons.iter().map(|x| x.season).collect();
        assert_eq!(seasons, [Some(1), Some(2), None]);
        assert_eq!(b.seasons[0].counts.remux, 1);
        assert_eq!(b.seasons[2].counts.review, 1);
    }
}
