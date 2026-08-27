use crate::install::scheduler::InstallResult;
use std::{collections::BTreeSet, time::Duration};

#[derive(Debug, Clone, Copy)]
pub struct InstallTimingSummary {
    pub total: Duration,
    pub download: Duration,
    pub cache_rebuild: Duration,
}

impl InstallTimingSummary {
    pub fn from_result(result: &InstallResult, total: Duration) -> Self {
        // Every package gets a `ghcr_bottle_download` node whether or not its artifact is
        // already cached — a cache hit still records an event, just a near-instant one. Using
        // *all* of them (e.g. the latest end timestamp) attributes however long the scheduler
        // took to get around to dispatching the very last one to "download" time, even when
        // nothing was actually fetched over the network. `add_dependency_edges`-adjacent code in
        // dag.rs only wires an "auth" edge onto the download nodes for artifacts that weren't
        // already cached at plan time, so that's the authoritative set of real downloads.
        let real_download_ids: BTreeSet<&str> = result
            .plan
            .edges
            .iter()
            .filter(|edge| edge.reason == "auth")
            .map(|edge| edge.target.as_str())
            .collect();

        let download_secs = union_duration(
            result
                .events
                .iter()
                .filter(|event| {
                    event.phase.is_none() && real_download_ids.contains(event.node_id.as_str())
                })
                .map(|event| (event.start, event.end)),
        );

        let cache_rebuild_secs = union_duration(
            result
                .events
                .iter()
                .filter(|event| {
                    event.phase.is_none() && event.node_id.starts_with("cache_postinstall:")
                })
                .map(|event| (event.start, event.end)),
        );

        let download = Duration::from_secs_f64(download_secs.max(0.0));
        let cache_rebuild = Duration::from_secs_f64(cache_rebuild_secs.max(0.0));

        Self {
            total,
            download,
            cache_rebuild,
        }
    }

    /// The phase breakdown (`Download`, `Cache rebuild`) as a display string,
    /// or `None` when neither bucket is nonzero — callers then omit the
    /// breakdown. Both figures are wall-clock spans covered by at least one
    /// real download / cache rebuild; they can overlap each other, so they do
    /// not sum to `total` (there is deliberately no remainder bucket).
    pub fn breakdown(&self) -> Option<String> {
        let has_download = self.download.as_secs_f64() > 0.0;
        let has_cache_rebuild = self.cache_rebuild.as_secs_f64() > 0.0;
        if !has_download && !has_cache_rebuild {
            return None;
        }

        let mut segments = Vec::new();
        if has_download {
            segments.push(format!("Download: {:.1}s", self.download.as_secs_f64()));
        }
        if has_cache_rebuild {
            segments.push(format!(
                "Cache rebuild: {:.1}s",
                self.cache_rebuild.as_secs_f64()
            ));
        }
        Some(segments.join(" · "))
    }
}

/// Total wall-clock time covered by a set of (start, end) intervals, merging overlaps so
/// concurrent work (e.g. several real downloads in flight at once) isn't double-counted.
fn union_duration(intervals: impl Iterator<Item = (f64, f64)>) -> f64 {
    let mut intervals = intervals.collect::<Vec<_>>();
    intervals.sort_by(|a, b| a.0.total_cmp(&b.0));

    let mut total = 0.0;
    let mut current: Option<(f64, f64)> = None;
    for (start, end) in intervals {
        current = Some(match current {
            None => (start, end),
            Some((cur_start, cur_end)) if start > cur_end => {
                total += cur_end - cur_start;
                (start, end)
            }
            Some((cur_start, cur_end)) => (cur_start, cur_end.max(end)),
        });
    }
    if let Some((start, end)) = current {
        total += end - start;
    }
    total
}

#[cfg(test)]
mod tests {
    use super::{union_duration, InstallTimingSummary};
    use std::time::Duration;

    #[test]
    fn union_duration_merges_overlaps_without_double_counting() {
        // [0,3] and [2,5] overlap -> merged span is [0,5] = 5.0, not 3.0+3.0=6.0.
        let total = union_duration(vec![(0.0, 3.0), (2.0, 5.0), (10.0, 11.0)].into_iter());
        assert_eq!(total, 6.0);
    }

    #[test]
    fn union_duration_empty_is_zero() {
        assert_eq!(union_duration(std::iter::empty()), 0.0);
    }

    fn summary(extra_phases: bool) -> InstallTimingSummary {
        if extra_phases {
            InstallTimingSummary {
                total: Duration::from_secs_f64(8.0),
                download: Duration::from_secs_f64(2.1),
                cache_rebuild: Duration::from_secs_f64(0.4),
            }
        } else {
            InstallTimingSummary {
                total: Duration::from_secs_f64(5.46),
                download: Duration::ZERO,
                cache_rebuild: Duration::ZERO,
            }
        }
    }

    #[test]
    fn breakdown_omitted_when_no_extra_phases() {
        assert_eq!(summary(false).breakdown(), None);
    }

    #[test]
    fn breakdown_lists_extra_phases() {
        assert_eq!(
            summary(true).breakdown().as_deref(),
            Some("Download: 2.1s · Cache rebuild: 0.4s")
        );
    }
}
