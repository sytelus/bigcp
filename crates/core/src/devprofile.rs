//! Deterministic static device-class profiles and bounded manual overrides.

use bigcp_win::{DeviceBus, DeviceInfo};
use serde::{Deserialize, Serialize};

use crate::error::BigcpError;
use crate::options::{DeviceClass, TuneOptions};

/// One side's initial static settings.
///
/// There is no queue-depth field: the shipped engine issues synchronous
/// sequential I/O (queue depth 1 by construction), so a queue-depth knob
/// would be a claim the implementation cannot honor.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SideProfile {
    /// Selected class.
    pub class: DeviceClass,
    /// high or low query confidence.
    pub confidence: String,
    /// Preferred chunk bytes.
    pub chunk_bytes: usize,
    /// Preferred concurrent streams.
    pub streams: usize,
    /// Small-file workers.
    pub workers: usize,
    /// Enumeration workers.
    pub enumeration_threads: usize,
}

/// Composed source/destination profile used by engines.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CopyProfile {
    /// Source-side settings.
    pub source: SideProfile,
    /// Destination-side settings.
    pub destination: SideProfile,
    /// Chunk clamped by both adapters.
    pub chunk_bytes: usize,
    /// Concurrent streams clamped by the slower side.
    pub streams: usize,
    /// Small-file workers from the slower side.
    pub workers: usize,
    /// True when source and destination extent sets intersect.
    pub same_physical_disk: bool,
}

/// Selects and composes profiles without probe traffic.
pub fn select_copy_profile(
    source_info: &DeviceInfo,
    destination_info: &DeviceInfo,
    source_override: DeviceClass,
    destination_override: DeviceClass,
    tune: &TuneOptions,
) -> Result<CopyProfile, BigcpError> {
    validate_tuning(tune)?;
    let cores = std::thread::available_parallelism().map_or(4, usize::from);
    let mut source = side_profile(source_info, source_override, cores, true);
    let mut destination = side_profile(destination_info, destination_override, cores, false);
    if let Some(workers) = tune.threads {
        source.workers = workers;
        destination.workers = workers;
    }
    if let Some(memory_bytes) = tune.memory_bytes {
        let large_threshold = tune.large_threshold.unwrap_or(16 * 1024 * 1024);
        let large_threshold = usize::try_from(large_threshold).map_err(|_| {
            BigcpError::Invalid("large-file threshold does not fit this address space".to_owned())
        })?;
        if memory_bytes < large_threshold {
            return Err(BigcpError::Invalid(format!(
                "memory budget {memory_bytes} is smaller than the large-file threshold {large_threshold}"
            )));
        }
        let budgeted_workers = (memory_bytes / large_threshold).clamp(1, 256);
        source.workers = source.workers.min(budgeted_workers);
        destination.workers = destination.workers.min(budgeted_workers);
    }
    let mut chunk_bytes = tune
        .chunk_bytes
        .unwrap_or_else(|| source.chunk_bytes.min(destination.chunk_bytes));
    for maximum in [
        source_info.maximum_transfer_length,
        destination_info.maximum_transfer_length,
    ]
    .into_iter()
    .flatten()
    {
        if maximum > 0 {
            chunk_bytes = chunk_bytes.min(maximum as usize);
        }
    }
    if let Some(memory_bytes) = tune.memory_bytes {
        chunk_bytes = chunk_bytes.min(memory_bytes);
    }
    if chunk_bytes == 0 {
        return Err(BigcpError::Invalid(
            "profile chunk size must be positive".to_owned(),
        ));
    }
    let streams = tune
        .streams
        .unwrap_or_else(|| source.streams.min(destination.streams));
    if streams == 0 {
        return Err(BigcpError::Invalid(
            "profile stream count must be positive".to_owned(),
        ));
    }
    let same_physical_disk = !source_info.disk_numbers.is_empty()
        && source_info
            .disk_numbers
            .iter()
            .any(|disk| destination_info.disk_numbers.contains(disk));
    // Small-file cost is destination-bound (creates, writes, closes all land
    // on the destination), so worker count follows the destination's row —
    // measured 2026-07-29: a min() composition let one low-confidence side
    // drag a 32-worker HDD destination down to 4 workers and forfeit the
    // close-overlap win (BENCHMARKS.md). The one exception is a seek-penalty
    // source, whose row stays authoritative: its random reads are the
    // scarcer resource.
    let workers = if source.class == DeviceClass::Hdd {
        source.workers.min(destination.workers)
    } else {
        destination.workers
    };
    Ok(CopyProfile {
        source,
        destination,
        chunk_bytes,
        streams,
        workers,
        same_physical_disk,
    })
}

fn validate_tuning(tune: &TuneOptions) -> Result<(), BigcpError> {
    const MIN_CHUNK_BYTES: usize = 64 * 1024;
    const MAX_CHUNK_BYTES: usize = 64 * 1024 * 1024;
    if let Some(workers) = tune.threads
        && !(1..=256).contains(&workers)
    {
        return Err(BigcpError::Invalid(
            "small-file worker count must be in 1..=256".to_owned(),
        ));
    }
    if let Some(streams) = tune.streams
        && !(1..=16).contains(&streams)
    {
        return Err(BigcpError::Invalid(
            "concurrent stream count must be in 1..=16".to_owned(),
        ));
    }
    if let Some(chunk) = tune.chunk_bytes
        && !(MIN_CHUNK_BYTES..=MAX_CHUNK_BYTES).contains(&chunk)
    {
        return Err(BigcpError::Invalid(format!(
            "chunk size must be in {MIN_CHUNK_BYTES}..={MAX_CHUNK_BYTES} bytes"
        )));
    }
    if tune.memory_bytes == Some(0) {
        return Err(BigcpError::Invalid(
            "memory budget must be positive".to_owned(),
        ));
    }
    if tune.large_threshold == Some(0) {
        return Err(BigcpError::Invalid(
            "large-file threshold must be positive".to_owned(),
        ));
    }
    if tune.checkpoint_threshold == Some(0) {
        return Err(BigcpError::Invalid(
            "checkpoint threshold must be positive".to_owned(),
        ));
    }
    Ok(())
}

fn side_profile(
    info: &DeviceInfo,
    requested: DeviceClass,
    cores: usize,
    source: bool,
) -> SideProfile {
    let class = if requested == DeviceClass::Auto {
        detected_class(info)
    } else {
        requested
    };
    let confidence = if requested != DeviceClass::Auto || info.high_confidence {
        "high"
    } else {
        "low"
    }
    .to_owned();
    let (chunk_bytes, streams, workers, enumeration_threads) = match class {
        DeviceClass::Nvme => (
            8 * 1024 * 1024,
            if cores >= 8 { 4 } else { 2 },
            (4 * cores).min(64),
            cores.min(16),
        ),
        DeviceClass::SataSsd => (8 * 1024 * 1024, 2, 32, 8),
        DeviceClass::UsbSsd => (4 * 1024 * 1024, 2, 16, 8),
        // Seek-penalty media get large sequential chunks and a single large
        // stream. Small-file workers stay HIGH on an HDD destination
        // (measured 2026-07-29, BENCHMARKS.md): small-file cost there is
        // metadata-bound — per-file CloseHandle is a ~2.3 ms write-through
        // round-trip on Quick-removal USB — and 32 outstanding closes
        // overlap in the device queue (19.1 s vs 31.5 s with 8 workers on
        // the 20k-file workload, beating robocopy). Reading from an HDD
        // source stays conservative (seek-bound, unmeasured).
        DeviceClass::Hdd => (
            16 * 1024 * 1024,
            1,
            if source { 4 } else { 32 },
            if source { 2 } else { 4 },
        ),
        DeviceClass::Auto | DeviceClass::Unknown => (4 * 1024 * 1024, 1, 4, 4),
    };
    SideProfile {
        class,
        confidence,
        chunk_bytes,
        streams,
        workers,
        enumeration_threads,
    }
}

fn detected_class(info: &DeviceInfo) -> DeviceClass {
    if info.incurs_seek_penalty == Some(true) {
        return DeviceClass::Hdd;
    }
    match info.bus {
        Some(DeviceBus::Nvme) => DeviceClass::Nvme,
        Some(DeviceBus::Sata) => DeviceClass::SataSsd,
        Some(DeviceBus::Usb) => DeviceClass::UsbSsd,
        // A positive "no seek penalty" answer means definitively
        // solid-state even when the bus is unrecognized — Intel VMD/RST
        // NVMe controllers report BusTypeRAID, which used to demote a fast
        // internal SSD to the conservative Unknown profile (found on this
        // project's own D: drive, 2026-07-29). The moderate SATA-SSD row is
        // the safe class-level choice without bus evidence for more.
        Some(DeviceBus::Other) | None if info.incurs_seek_penalty == Some(false) => {
            DeviceClass::SataSsd
        }
        Some(DeviceBus::Other) | None => DeviceClass::Unknown,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn no_seek_penalty_on_unrecognized_bus_is_ssd_class() {
        // Regression pin for the VMD/RST misclassification (2026-07-29):
        // a drive that positively reports no seek penalty must never fall
        // to the Unknown profile just because its bus is unrecognized.
        let probe = |seek: Option<bool>| DeviceInfo {
            disk_numbers: vec![1],
            incurs_seek_penalty: seek,
            bus: None,
            maximum_transfer_length: None,
            logical_sector: Some(512),
            physical_sector: None,
            write_cache_enabled: None,
            high_confidence: false,
        };
        assert_eq!(
            super::detected_class(&probe(Some(false))),
            DeviceClass::SataSsd
        );
        assert_eq!(super::detected_class(&probe(Some(true))), DeviceClass::Hdd);
        assert_eq!(super::detected_class(&probe(None)), DeviceClass::Unknown);
    }

    use super::select_copy_profile;
    use crate::options::{DeviceClass, TuneOptions};
    use bigcp_win::{DeviceBus, DeviceInfo};

    fn nvme() -> DeviceInfo {
        DeviceInfo {
            disk_numbers: vec![0],
            incurs_seek_penalty: Some(false),
            bus: Some(DeviceBus::Nvme),
            maximum_transfer_length: Some(4 * 1024 * 1024),
            logical_sector: Some(512),
            physical_sector: Some(4096),
            write_cache_enabled: Some(true),
            high_confidence: true,
        }
    }

    #[test]
    fn composition_is_deterministic_and_mtl_clamped() {
        let first = select_copy_profile(
            &nvme(),
            &nvme(),
            DeviceClass::Auto,
            DeviceClass::Auto,
            &TuneOptions::default(),
        );
        let second = select_copy_profile(
            &nvme(),
            &nvme(),
            DeviceClass::Auto,
            DeviceClass::Auto,
            &TuneOptions::default(),
        );
        assert!(first.is_ok());
        assert_eq!(
            first.ok().map(|profile| profile.chunk_bytes),
            second.ok().map(|profile| profile.chunk_bytes)
        );
    }

    #[test]
    fn unsafe_manual_ranges_are_rejected_by_the_library_api() {
        for tune in [
            TuneOptions {
                threads: Some(0),
                ..TuneOptions::default()
            },
            TuneOptions {
                streams: Some(17),
                ..TuneOptions::default()
            },
            TuneOptions {
                chunk_bytes: Some(4096),
                ..TuneOptions::default()
            },
            TuneOptions {
                checkpoint_threshold: Some(0),
                ..TuneOptions::default()
            },
        ] {
            assert!(
                select_copy_profile(
                    &nvme(),
                    &nvme(),
                    DeviceClass::Auto,
                    DeviceClass::Auto,
                    &tune,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn memory_budget_caps_workers_and_chunks() {
        let tune = TuneOptions {
            chunk_bytes: Some(8 * 1024 * 1024),
            threads: Some(16),
            memory_bytes: Some(8 * 1024 * 1024),
            large_threshold: Some(4 * 1024 * 1024),
            ..TuneOptions::default()
        };
        let profile = select_copy_profile(
            &nvme(),
            &nvme(),
            DeviceClass::Auto,
            DeviceClass::Auto,
            &tune,
        );
        assert!(profile.is_ok());
        assert!(
            profile.is_ok_and(|value| value.workers == 2 && value.chunk_bytes <= 8 * 1024 * 1024)
        );
    }
}
