#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LiveHnswS3VersionDisposition {
    Current { clear_stale_retired_marker: bool },
    HistoricalRetired,
    FutureSpeculative { clear_stale_retired_marker: bool },
}

pub(super) fn classify_live_hnsw_s3_version(
    current_version: u64,
    object_version: u64,
    retired_marker_present: bool,
) -> LiveHnswS3VersionDisposition {
    use std::cmp::Ordering;

    match object_version.cmp(&current_version) {
        Ordering::Equal => LiveHnswS3VersionDisposition::Current {
            clear_stale_retired_marker: retired_marker_present,
        },
        Ordering::Less => LiveHnswS3VersionDisposition::HistoricalRetired,
        Ordering::Greater => LiveHnswS3VersionDisposition::FutureSpeculative {
            clear_stale_retired_marker: retired_marker_present,
        },
    }
}
