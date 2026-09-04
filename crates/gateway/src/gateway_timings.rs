use serde::Serialize;
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

const SCHEMA: &str = "sprite-desktop-gateway-stage-timings-v1";
const MAX_RECORDS: usize = 65_536;

fn decimal<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&value.to_string())
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub(crate) enum TimingRecord {
    RtpAuComplete {
        #[serde(serialize_with = "decimal")]
        timestamp_nanos: u64,
        rtp_timestamp: u32,
        ssrc: u32,
        byte_length: usize,
    },
    MetadataMatch {
        #[serde(serialize_with = "decimal")]
        timestamp_nanos: u64,
        sequence: u64,
        generation: u32,
        rtp_timestamp: u32,
        ssrc: u32,
    },
    HubEnqueue {
        #[serde(serialize_with = "decimal")]
        timestamp_nanos: u64,
        sequence: u64,
        generation: u32,
    },
    SocketWriteStart {
        #[serde(serialize_with = "decimal")]
        timestamp_nanos: u64,
        sequence: u64,
        generation: u32,
        connection_id: u64,
        byte_length: usize,
    },
    SocketWriteEnd {
        #[serde(serialize_with = "decimal")]
        timestamp_nanos: u64,
        sequence: u64,
        generation: u32,
        connection_id: u64,
        byte_length: usize,
    },
}

#[derive(Clone)]
pub(crate) struct GatewayTimings {
    trace: Arc<Mutex<Trace>>,
    active_epoch: Arc<AtomicU64>,
    file: Arc<Mutex<File>>,
}
#[derive(Clone, Copy)]
pub(crate) struct TimingSample {
    epoch: u64,
    timestamp_nanos: u64,
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

impl GatewayTimings {
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
            .expect("gateway timing epoch exhausted");
        trace.active_epoch = Some(trace.epoch);
        // A producer that acquires this epoch may sample the clock without taking
        // the trace mutex. publish() validates the epoch again under that mutex.
        self.active_epoch.store(trace.epoch, Ordering::Release);
    }

    pub(crate) fn sample(&self) -> Option<TimingSample> {
        self.sample_with(monotonic_nanos)
    }

    fn sample_with<F>(&self, clock: F) -> Option<TimingSample>
    where
        F: FnOnce() -> Option<u64>,
    {
        // Acquire pairs with start/stop's Release stores. Zero is an early exit:
        // inactive producers do not call the clock, builder, or trace mutex.
        let epoch = self.active_epoch.load(Ordering::Acquire);
        if epoch == 0 {
            return None;
        }
        Some(TimingSample {
            epoch,
            timestamp_nanos: clock()?,
        })
    }

    pub(crate) fn publish<F>(&self, sample: TimingSample, build: F) -> bool
    where
        F: FnOnce(u64) -> TimingRecord,
    {
        let mut trace = self.trace.lock().unwrap();
        // Sampling can race stop/start. The mutex check keeps an old producer
        // from publishing into the next recording epoch.
        if trace.active_epoch != Some(sample.epoch) {
            return false;
        }
        if trace.records.len() < MAX_RECORDS {
            trace.records.push(build(sample.timestamp_nanos));
        } else {
            trace.overflow_count = trace.overflow_count.saturating_add(1);
        }
        true
    }

    pub(crate) fn record<F>(&self, build: F)
    where
        F: FnOnce(u64) -> TimingRecord,
    {
        if let Some(sample) = self.sample() {
            self.publish(sample, build);
        }
    }

    pub(crate) fn record_in_epoch<F>(&self, epoch: TimingSample, build: F)
    where
        F: FnOnce(u64) -> TimingRecord,
    {
        // Check before reading the clock, then let publish() close the race with
        // a concurrent stop/start transition.
        if self.active_epoch.load(Ordering::Acquire) != epoch.epoch {
            return;
        }
        if let Some(sample) = self.sample()
            && sample.epoch == epoch.epoch
        {
            self.publish(sample, build);
        }
    }

    #[cfg(test)]
    pub(crate) fn records(&self) -> Vec<TimingRecord> {
        self.trace.lock().unwrap().records.clone()
    }

    pub(crate) fn stop_and_dump(&self) -> io::Result<()> {
        let snapshot = {
            let mut trace = self.trace.lock().unwrap();
            trace.active_epoch = None;
            // Release makes the inactive state visible before later producers
            // decide whether to sample the clock or enter the trace mutex.
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

fn monotonic_nanos() -> Option<u64> {
    let timestamp = nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC).ok()?;
    let seconds = u64::try_from(timestamp.tv_sec()).ok()?;
    let nanos = u64::try_from(timestamp.tv_nsec()).ok()?;
    seconds.checked_mul(1_000_000_000)?.checked_add(nanos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "gateway-timings-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
    fn record(sequence: u64) -> TimingRecord {
        TimingRecord::HubEnqueue {
            timestamp_nanos: sequence,
            sequence,
            generation: 1,
        }
    }

    #[test]
    fn file_is_exclusive_private_and_trace_is_bounded() {
        let path = path("bounds");
        let timings = GatewayTimings::open(Some(&path)).unwrap().unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(GatewayTimings::open(Some(&path)).is_err());
        timings.start();
        for sequence in 0..MAX_RECORDS as u64 + 2 {
            timings.record(|_| record(sequence));
        }
        let trace = timings.trace.lock().unwrap();
        assert_eq!(trace.records.len(), MAX_RECORDS);
        assert_eq!(trace.overflow_count, 2);
        drop(trace);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn inactive_sampling_skips_clock_builder_and_mutex() {
        let path = path("inactive");
        let timings = GatewayTimings::open(Some(&path)).unwrap().unwrap();
        let trace = timings.trace.lock().unwrap();
        let mut clock_ran = false;
        assert!(
            timings
                .sample_with(|| {
                    clock_ran = true;
                    Some(1)
                })
                .is_none()
        );
        let mut builder_ran = false;
        timings.record(|_| {
            builder_ran = true;
            record(0)
        });
        assert!(!clock_ran && !builder_ran);
        drop(trace);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn sampled_old_epoch_cannot_publish_after_stop_and_start() {
        let path = path("epoch");
        let timings = GatewayTimings::open(Some(&path)).unwrap().unwrap();
        timings.start();
        let old = timings.sample_with(|| Some(1)).unwrap();
        timings.stop_and_dump().unwrap();
        timings.start();
        let mut builder_ran = false;
        assert!(!timings.publish(old, |_| {
            builder_ran = true;
            record(1)
        }));
        timings.record_in_epoch(old, |_| record(2));
        assert!(!builder_ran);
        assert!(timings.trace.lock().unwrap().records.is_empty());
        fs::remove_file(path).unwrap();
    }
}
