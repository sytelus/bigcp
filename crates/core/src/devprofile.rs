//! Deterministic static device-class profiles and bounded manual overrides.

use bigcp_win::{DeviceBus, DeviceInfo, EndpointKind};
use serde::{Deserialize, Serialize};

use crate::error::BigcpError;
use crate::options::{DeviceClass, TuneOptions};
use crate::transport::TransportProfile;

const DEFAULT_SAME_SPINDLE_BURST_BYTES: usize = 256 * 1024 * 1024;
const MIN_SAME_SPINDLE_BURST_BYTES: usize = 1024 * 1024;
const MAX_SAME_SPINDLE_BURST_BYTES: usize = 1024 * 1024 * 1024;
const MIN_CHUNK_BYTES: usize = 64 * 1024;
const MAX_CHUNK_BYTES: usize = 64 * 1024 * 1024;

/// One side's initial static settings.
///
/// There is no queue-depth field: the shipped engine issues synchronous
/// sequential I/O (queue depth 1 by construction), so a queue-depth knob
/// would be a claim the implementation cannot honor.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SideProfile {
    /// Local, generic UNC, or WSL UNC access path.
    #[serde(default)]
    pub endpoint: EndpointKind,
    /// Selected class.
    pub class: DeviceClass,
    /// high or low query confidence.
    pub confidence: String,
    /// Preferred chunk bytes.
    pub chunk_bytes: usize,
    /// Small-file workers.
    pub workers: usize,
}

/// Composed source/destination profile used by the copy engine.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CopyProfile {
    /// Source-side settings.
    pub source: SideProfile,
    /// Destination-side settings.
    pub destination: SideProfile,
    /// Chunk clamped by both adapters.
    pub chunk_bytes: usize,
    /// Composed small-file worker count (destination-led, source-HDD-capped).
    pub workers: usize,
    /// True when source and destination extent sets intersect.
    pub same_physical_disk: bool,
    /// Data transport selected from physical topology and seek behavior.
    #[serde(default)]
    pub transport: TransportProfile,
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
    let large_threshold = usize::try_from(tune.large_threshold.unwrap_or(16 * 1024 * 1024))
        .map_err(|_| {
            BigcpError::Invalid("large-file threshold does not fit this address space".to_owned())
        })?;
    let same_physical_disk = !source_info.disk_numbers.is_empty()
        && source_info
            .disk_numbers
            .iter()
            .any(|disk| destination_info.disk_numbers.contains(disk));
    let same_spindle = same_physical_disk
        && (source.class == DeviceClass::Hdd || destination.class == DeviceClass::Hdd);
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
        let chunk_budget = if same_spindle {
            memory_bytes
        } else {
            memory_bytes
                .checked_sub(large_threshold)
                .filter(|available| *available >= MIN_CHUNK_BYTES)
                .ok_or_else(|| {
                    BigcpError::Invalid(format!(
                        "memory budget {memory_bytes} must hold one small-file buffer ({large_threshold}) and a minimum coordinator chunk ({MIN_CHUNK_BYTES})"
                    ))
                })?
        };
        chunk_bytes = chunk_bytes.min(chunk_budget);
    }
    if chunk_bytes == 0 {
        return Err(BigcpError::Invalid(
            "profile chunk size must be positive".to_owned(),
        ));
    }
    // Small-file cost is destination-bound (creates, writes, closes all land
    // on the destination), so worker count follows the destination's row —
    // measured 2026-07-29: a min() composition let one low-confidence side
    // drag a 32-worker HDD destination down to 4 workers and forfeit the
    // close-overlap win (BENCHMARKS.md). The one exception is a seek-penalty
    // source, whose row stays authoritative: its random reads are the
    // scarcer resource.
    let mut workers = if source.class == DeviceClass::Hdd || source_info.endpoint.is_remote() {
        source.workers.min(destination.workers)
    } else {
        destination.workers
    };
    if !same_spindle && let Some(memory_bytes) = tune.memory_bytes {
        // Standard transport can run one coordinator-owned chunk transfer
        // while small workers each hold one threshold-bounded file. Reserve
        // that chunk before deriving the worker cap so `mem=` is an actual
        // aggregate copy-buffer bound, not merely a per-allocation bound.
        let worker_bytes = memory_bytes.saturating_sub(chunk_bytes);
        if worker_bytes < large_threshold {
            return Err(BigcpError::Invalid(format!(
                "memory budget {memory_bytes} must hold one chunk ({chunk_bytes}) and one small-file buffer ({large_threshold})"
            )));
        }
        let budgeted_workers = (worker_bytes / large_threshold).clamp(1, 256);
        source.workers = source.workers.min(budgeted_workers);
        destination.workers = destination.workers.min(budgeted_workers);
        workers = workers.min(budgeted_workers);
    }
    let transport = if same_spindle {
        if tune.threads.is_some_and(|workers| workers != 1) {
            return Err(BigcpError::Invalid(
                "same-spindle phased scheduling requires threads=1; omit threads or set it to 1"
                    .to_owned(),
            ));
        }
        let mut burst_bytes = tune
            .same_spindle_burst_bytes
            .unwrap_or(DEFAULT_SAME_SPINDLE_BURST_BYTES);
        if let Some(memory_bytes) = tune.memory_bytes {
            burst_bytes = burst_bytes.min(memory_bytes);
        }
        let minimum = chunk_bytes.max(large_threshold);
        if burst_bytes < minimum {
            return Err(BigcpError::Invalid(format!(
                "same-spindle burst {burst_bytes} is smaller than the required chunk/whole-file buffer {minimum}"
            )));
        }
        // One phased scheduler owns rotational source/destination I/O. More
        // workers would interleave its read and write phases and recreate the
        // head-seek storm this profile exists to prevent.
        workers = 1;
        TransportProfile::same_spindle(burst_bytes)
    } else {
        TransportProfile::standard(chunk_bytes)
    };
    Ok(CopyProfile {
        source,
        destination,
        chunk_bytes,
        workers,
        same_physical_disk,
        transport,
    })
}

fn validate_tuning(tune: &TuneOptions) -> Result<(), BigcpError> {
    if let Some(workers) = tune.threads
        && !(1..=256).contains(&workers)
    {
        return Err(BigcpError::Invalid(
            "small-file worker count must be in 1..=256".to_owned(),
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
    if let Some(burst) = tune.same_spindle_burst_bytes
        && !(MIN_SAME_SPINDLE_BURST_BYTES..=MAX_SAME_SPINDLE_BURST_BYTES).contains(&burst)
    {
        return Err(BigcpError::Invalid(format!(
            "same-spindle burst must be in {MIN_SAME_SPINDLE_BURST_BYTES}..={MAX_SAME_SPINDLE_BURST_BYTES} bytes"
        )));
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
    let (chunk_bytes, workers) = match (requested == DeviceClass::Auto, info.endpoint) {
        // Remote roots do not expose local device topology. These immutable
        // profiles favor fewer, larger protocol operations while retaining
        // enough small-file concurrency to cover redirector latency. WSL is
        // intentionally more conservative because every call crosses its 9P
        // Windows/Linux translation boundary.
        (true, EndpointKind::Wsl) => (4 * 1024 * 1024, 8),
        (true, EndpointKind::Unc) => (8 * 1024 * 1024, 16),
        (_, _) => match class {
            DeviceClass::Nvme => (8 * 1024 * 1024, (4 * cores).min(64)),
            DeviceClass::SataSsd => (8 * 1024 * 1024, 32),
            DeviceClass::UsbSsd => (4 * 1024 * 1024, 16),
            // Seek-penalty media get large sequential chunks. Small-file workers
            // stay HIGH on an HDD destination
            // (measured 2026-07-29, BENCHMARKS.md): small-file cost there is
            // metadata-bound — per-file CloseHandle is a ~2.3 ms write-through
            // round-trip on Quick-removal USB — and 32 outstanding closes
            // overlap in the device queue (19.1 s vs 31.5 s with 8 workers on
            // the 20k-file workload, beating robocopy). Reading from an HDD
            // source stays conservative (seek-bound, unmeasured).
            DeviceClass::Hdd => (16 * 1024 * 1024, if source { 4 } else { 32 }),
            DeviceClass::Auto | DeviceClass::Unknown => (4 * 1024 * 1024, 4),
        },
    };
    SideProfile {
        endpoint: info.endpoint,
        class,
        confidence,
        chunk_bytes,
        workers,
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
            endpoint: EndpointKind::Local,
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
    use bigcp_win::{DeviceBus, DeviceInfo, EndpointKind};

    fn nvme() -> DeviceInfo {
        DeviceInfo {
            endpoint: EndpointKind::Local,
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
                chunk_bytes: Some(4096),
                ..TuneOptions::default()
            },
            TuneOptions {
                checkpoint_threshold: Some(0),
                ..TuneOptions::default()
            },
            TuneOptions {
                same_spindle_burst_bytes: Some(super::MIN_SAME_SPINDLE_BURST_BYTES - 1),
                ..TuneOptions::default()
            },
            TuneOptions {
                same_spindle_burst_bytes: Some(super::MAX_SAME_SPINDLE_BURST_BYTES + 1),
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
            profile.is_ok_and(|value| value.workers == 1 && value.chunk_bytes <= 8 * 1024 * 1024)
        );
    }

    #[test]
    fn standard_memory_budget_reserves_the_concurrent_coordinator_chunk() {
        let mut unlimited = nvme();
        unlimited.maximum_transfer_length = None;
        let bounded = TuneOptions {
            chunk_bytes: Some(8 * 1024 * 1024),
            memory_bytes: Some(8 * 1024 * 1024),
            large_threshold: Some(4 * 1024 * 1024),
            ..TuneOptions::default()
        };
        let profile = select_copy_profile(
            &unlimited,
            &unlimited,
            DeviceClass::Auto,
            DeviceClass::Auto,
            &bounded,
        );
        assert!(
            profile
                .is_ok_and(|value| { value.chunk_bytes == 4 * 1024 * 1024 && value.workers == 1 })
        );

        let too_small = TuneOptions {
            memory_bytes: Some(4 * 1024 * 1024),
            large_threshold: Some(4 * 1024 * 1024),
            ..TuneOptions::default()
        };
        assert!(
            select_copy_profile(
                &unlimited,
                &unlimited,
                DeviceClass::Auto,
                DeviceClass::Auto,
                &too_small,
            )
            .is_err_and(|error| error.to_string().contains("must hold"))
        );
    }

    #[test]
    fn shared_rotational_disk_selects_one_phased_scheduler() {
        let mut source = nvme();
        source.incurs_seek_penalty = Some(true);
        source.bus = Some(DeviceBus::Usb);
        let mut destination = source.clone();
        destination.disk_numbers = vec![0, 2];
        let profile = select_copy_profile(
            &source,
            &destination,
            DeviceClass::Auto,
            DeviceClass::Auto,
            &TuneOptions::default(),
        );
        assert!(profile.is_ok());
        let Some(profile) = profile.ok() else {
            return;
        };
        assert!(profile.same_physical_disk);
        assert!(profile.transport.is_same_spindle());
        assert_eq!(profile.workers, 1);
        assert_eq!(profile.transport.burst_bytes, 256 * 1024 * 1024);
    }

    #[test]
    fn shared_solid_state_disk_keeps_standard_transport() {
        let profile = select_copy_profile(
            &nvme(),
            &nvme(),
            DeviceClass::Auto,
            DeviceClass::Auto,
            &TuneOptions::default(),
        );
        assert!(profile.is_ok());
        let Some(profile) = profile.ok() else {
            return;
        };
        assert!(profile.same_physical_disk);
        assert!(!profile.transport.is_same_spindle());
        assert!(profile.workers > 1);
        assert_eq!(profile.transport.burst_bytes, profile.chunk_bytes);
    }

    #[test]
    fn same_spindle_rejects_conflicting_parallel_worker_override() {
        let mut source = nvme();
        source.incurs_seek_penalty = Some(true);
        let tune = TuneOptions {
            threads: Some(2),
            ..TuneOptions::default()
        };
        let result = select_copy_profile(
            &source,
            &source,
            DeviceClass::Auto,
            DeviceClass::Auto,
            &tune,
        );
        assert!(result.is_err_and(|error| error.to_string().contains("requires threads=1")));
    }

    #[test]
    fn remote_source_caps_local_workers_and_wsl_is_independently_profiled() {
        let mut source = nvme();
        source.endpoint = EndpointKind::Wsl;
        source.disk_numbers.clear();
        source.incurs_seek_penalty = None;
        source.bus = None;
        source.high_confidence = false;
        let profile = select_copy_profile(
            &source,
            &nvme(),
            DeviceClass::Auto,
            DeviceClass::Auto,
            &TuneOptions::default(),
        );
        assert!(profile.is_ok_and(|value| {
            value.source.endpoint == EndpointKind::Wsl
                && value.workers == 8
                && value.chunk_bytes == 4 * 1024 * 1024
                && !value.transport.is_same_spindle()
        }));
    }
}
