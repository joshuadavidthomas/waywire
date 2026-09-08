use std::{
    fs::{File, OpenOptions},
    io::{self, Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use serde::Serialize;

const SCHEMA: &str = "sprite-desktop-streamd-stage-timings-v2";
// Twelve records per frame leave room for 2,730 fully instrumented frames, more
// than the 1,530 frames expected from the 51 FPS, 30-second probe.
const MAX_RECORDS: usize = 32_768;

fn serialize_u64_decimal<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&value.to_string())
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub(crate) enum TimingRecord {
    WaylandReady {
        #[serde(serialize_with = "serialize_u64_decimal")]
        timestamp_nanos: u64,
        sequence: u64,
        generation: u32,
        #[serde(serialize_with = "serialize_u64_decimal")]
        protocol_ready_nanos: u64,
    },
    CaptureRequestComplete {
        #[serde(serialize_with = "serialize_u64_decimal")]
        timestamp_nanos: u64,
        sequence: u64,
        generation: u32,
    },
    CaptureRequestFlushComplete {
        #[serde(serialize_with = "serialize_u64_decimal")]
        timestamp_nanos: u64,
        sequence: u64,
        generation: u32,
    },
    CopyAuthorized {
        #[serde(serialize_with = "serialize_u64_decimal")]
        timestamp_nanos: u64,
        sequence: u64,
        generation: u32,
    },
    CopyFlushComplete {
        #[serde(serialize_with = "serialize_u64_decimal")]
        timestamp_nanos: u64,
        sequence: u64,
        generation: u32,
    },
    SubmitStart {
        #[serde(serialize_with = "serialize_u64_decimal")]
        timestamp_nanos: u64,
        sequence: u64,
        generation: u32,
    },
    SubmitEnd {
        #[serde(serialize_with = "serialize_u64_decimal")]
        timestamp_nanos: u64,
        sequence: u64,
        generation: u32,
        outcome: SubmissionOutcome,
    },
    PipeWriteStart {
        #[serde(serialize_with = "serialize_u64_decimal")]
        timestamp_nanos: u64,
        sequence: u64,
        generation: u32,
    },
    PipeWriteEnd {
        #[serde(serialize_with = "serialize_u64_decimal")]
        timestamp_nanos: u64,
        sequence: u64,
        generation: u32,
    },
    NotificationEnqueue {
        #[serde(serialize_with = "serialize_u64_decimal")]
        timestamp_nanos: u64,
        sequence: u64,
        generation: u32,
    },
    MetadataDispatch {
        #[serde(serialize_with = "serialize_u64_decimal")]
        timestamp_nanos: u64,
        sequence: u64,
        generation: u32,
    },
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SubmissionOutcome {
    Queued,
    ReplacedPending,
}

#[derive(Clone)]
pub(crate) struct StageTimings {
    trace: Arc<Mutex<Trace>>,
    active_epoch: Arc<AtomicU64>,
    file: Arc<Mutex<File>>,
}

#[derive(Clone, Copy)]
pub(crate) struct TimingGuard {
    epoch: u64,
}

#[derive(Clone, Copy)]
pub(crate) struct TimingSample {
    timestamp_nanos: u64,
    guard: TimingGuard,
}

struct Trace {
    epoch: u64,
    active_epoch: Option<u64>,
    overflow_count: u64,
    records: Vec<TimingRecord>,
}

#[derive(Serialize)]
struct Snapshot {
    schema: &'static str,
    pid: u32,
    overflow_count: u64,
    records: Vec<TimingRecord>,
}

impl StageTimings {
    pub(crate) fn open(path: Option<&Path>) -> io::Result<Option<Self>> {
        path.map(|path| {
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)?;
            Ok(Self {
                trace: Arc::new(Mutex::new(Trace {
                    epoch: 0,
                    active_epoch: None,
                    overflow_count: 0,
                    records: Vec::with_capacity(MAX_RECORDS),
                })),
                active_epoch: Arc::new(AtomicU64::new(0)),
                file: Arc::new(Mutex::new(file)),
            })
        })
        .transpose()
    }

    pub(crate) fn start(&self) {
        let mut trace = self.trace.lock().unwrap();
        trace.records.clear();
        trace.overflow_count = 0;
        trace.epoch = trace
            .epoch
            .checked_add(1)
            .expect("stage timing epoch exhausted");
        trace.active_epoch = Some(trace.epoch);
        // Release publishes the reset trace state before producers observe this epoch.
        self.active_epoch.store(trace.epoch, Ordering::Release);
    }

    pub(crate) fn guard(&self) -> Option<TimingGuard> {
        // Acquire pairs with start's Release. Zero means the fast path returns
        // without reading a clock, constructing a record, or taking a mutex.
        let epoch = self.active_epoch.load(Ordering::Acquire);
        (epoch != 0).then_some(TimingGuard { epoch })
    }

    pub(crate) fn sample(&self) -> Option<TimingSample> {
        let guard = self.guard()?;
        Some(TimingSample {
            timestamp_nanos: monotonic_nanos()?,
            guard,
        })
    }

    pub(crate) fn record_now<F>(&self, build: F)
    where
        F: FnOnce(u64) -> TimingRecord,
    {
        if let Some(sample) = self.sample() {
            self.record_sample(sample, build);
        }
    }

    pub(crate) fn record_at<F>(&self, guard: TimingGuard, timestamp_nanos: u64, build: F)
    where
        F: FnOnce(u64) -> TimingRecord,
    {
        self.publish(guard.epoch, build(timestamp_nanos));
    }

    pub(crate) fn record_sample<F>(&self, sample: TimingSample, build: F)
    where
        F: FnOnce(u64) -> TimingRecord,
    {
        self.publish(sample.guard.epoch, build(sample.timestamp_nanos));
    }

    fn publish(&self, epoch: u64, record: TimingRecord) {
        let mut trace = self.trace.lock().unwrap();
        if trace.active_epoch != Some(epoch) {
            return;
        }
        if trace.records.len() < MAX_RECORDS {
            trace.records.push(record);
        } else {
            trace.overflow_count = trace.overflow_count.saturating_add(1);
        }
    }

    pub(crate) fn stop_and_dump(&self) -> io::Result<()> {
        let snapshot = {
            let mut trace = self.trace.lock().unwrap();
            trace.active_epoch = None;
            // Release makes inactivity visible before the trace lock is released.
            self.active_epoch.store(0, Ordering::Release);
            Snapshot {
                schema: SCHEMA,
                pid: std::process::id(),
                overflow_count: trace.overflow_count,
                records: trace.records.clone(),
            }
        };
        let bytes = serde_json::to_vec(&snapshot)?;
        let mut file = self.file.lock().unwrap();
        file.seek(SeekFrom::Start(0))?;
        file.set_len(0)?;
        file.write_all(&bytes)?;
        file.flush()
    }
}

pub(crate) fn monotonic_nanos() -> Option<u64> {
    let timestamp = nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC).ok()?;
    let seconds = u64::try_from(timestamp.tv_sec()).ok()?;
    let nanos = u64::try_from(timestamp.tv_nsec()).ok()?;
    seconds.checked_mul(1_000_000_000)?.checked_add(nanos)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use serde_json::Value;

    use super::*;

    fn unused_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "streamd-stage-timings-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn record(sequence: u64) -> TimingRecord {
        TimingRecord::SubmitStart {
            timestamp_nanos: sequence + 100,
            sequence,
            generation: 7,
        }
    }

    #[test]
    fn disabled_does_not_open_a_file() {
        assert!(StageTimings::open(None).unwrap().is_none());
    }

    #[test]
    fn output_file_must_be_private_to_this_process() {
        let path = unused_path("exclusive");
        fs::write(&path, b"keep").unwrap();
        assert!(StageTimings::open(Some(&path)).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"keep");
        fs::remove_file(path).unwrap();

        let path = unused_path("permissions");
        let _timings = StageTimings::open(Some(&path)).unwrap().unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn capacity_covers_probe_and_overflow_counts_drops_at_actual_cap() {
        let path = unused_path("capacity");
        let timings = StageTimings::open(Some(&path)).unwrap().unwrap();
        assert_eq!(MAX_RECORDS, 32_768);
        let capacity = timings.trace.lock().unwrap().records.capacity();
        assert!(capacity >= 12 * 51 * 30);
        timings.start();
        for sequence in 0..MAX_RECORDS as u64 + 3 {
            timings.record_now(|_| record(sequence));
        }
        {
            let trace = timings.trace.lock().unwrap();
            assert_eq!(trace.records.len(), MAX_RECORDS);
            assert_eq!(trace.overflow_count, 3);
        }
        timings.start();
        let trace = timings.trace.lock().unwrap();
        assert_eq!(trace.records.capacity(), capacity);
        assert!(trace.records.is_empty());
        assert_eq!(trace.overflow_count, 0);
        drop(trace);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn snapshot_contains_only_timing_and_capture_correlation() {
        let path = unused_path("snapshot");
        let timings = StageTimings::open(Some(&path)).unwrap().unwrap();
        timings.start();
        timings.record_now(|_| record(42));
        timings.stop_and_dump().unwrap();

        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["schema"], SCHEMA);
        assert_eq!(value["pid"], std::process::id());
        assert_eq!(value["overflow_count"], 0);
        assert_eq!(value["records"][0]["stage"], "submit_start");
        assert_eq!(value["records"][0]["timestamp_nanos"], "142");
        assert_eq!(value["records"][0]["sequence"], 42);
        assert_eq!(value["records"][0]["generation"], 7);
        assert!(value.to_string().find("input").is_none());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn inactive_record_does_not_run_builder_even_while_trace_mutex_is_held() {
        let path = unused_path("inactive-lazy");
        let timings = StageTimings::open(Some(&path)).unwrap().unwrap();
        let trace = timings.trace.lock().unwrap();
        let mut ran = false;
        timings.record_now(|_| {
            ran = true;
            record(1)
        });
        assert!(!ran);
        drop(trace);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn stale_producer_cannot_publish_into_a_new_recording() {
        let path = unused_path("epoch");
        let timings = StageTimings::open(Some(&path)).unwrap().unwrap();
        timings.start();
        let stale = timings.sample().unwrap();
        timings.stop_and_dump().unwrap();
        timings.start();
        timings.record_sample(stale, |_| record(1));
        timings.record_now(|_| record(2));
        let trace = timings.trace.lock().unwrap();
        assert_eq!(trace.records.len(), 1);
        assert!(matches!(
            trace.records[0],
            TimingRecord::SubmitStart { sequence: 2, .. }
        ));
        drop(trace);
        fs::remove_file(path).unwrap();
    }
}
