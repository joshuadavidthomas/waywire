//! Frame latency traces for the benchmark's viewer latency breakdown. Each stage
//! of a frame, from render in the compositor through encoding here, is logged on
//! the `waywire_latency` target against the frame capture clock.

/// `CLOCK_MONOTONIC` in nanoseconds: the clock of frame capture timestamps.
#[must_use]
pub fn monotonic_nanos() -> u64 {
    nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC).map_or(0, |timestamp| {
        u64::try_from(timestamp.tv_sec())
            .unwrap_or(0)
            .saturating_mul(1_000_000_000)
            .saturating_add(u64::try_from(timestamp.tv_nsec()).unwrap_or(0))
    })
}

/// Trace a frame stage that began at `start_ns` and ends now.
pub fn trace_frame(stage: &'static str, sequence: u64, start_ns: u64) {
    tracing::debug!(
        target: "waywire_latency",
        stage,
        sequence,
        start_ns,
        at_ns = monotonic_nanos(),
    );
}
