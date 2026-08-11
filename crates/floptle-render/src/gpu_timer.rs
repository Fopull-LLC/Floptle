//! **Where the frame actually goes**, measured on the GPU rather than guessed.
//!
//! Until this existed the engine could report one number — how long a frame took
//! wall-clock — and nothing about what it was doing. That is enough to know a
//! scene is slow and useless for knowing why, so every performance question came
//! down to commenting a feature out and looking at the number again. That method
//! has already produced one wrong answer in this codebase: under vsync every
//! toggle reads the same, because the display quantises the result, and a whole
//! afternoon went into features that turned out to cost nothing.
//!
//! **Timestamps, not a wall clock.** The CPU records commands and moves on; the
//! GPU runs them later, at its own pace. Timing the recording measures how fast
//! the encoder ran, which is nearly always fast and nearly never the answer.
//! `write_timestamp` puts a marker *in the command stream*, so the interval
//! between two of them is time the GPU spent, in order, on the work between.
//!
//! **One mark per group, on its own encoder.** Every pass in this crate makes and
//! submits its own encoder, so there is no shared one to bracket. A mark is
//! therefore a submission of its own carrying a single command — cheap, ordered
//! with everything around it, and requiring no change inside any pass. Nothing is
//! submitted at all while the timer is off, which is its state unless somebody
//! opens the panel.
//!
//! **A frame's readings arrive a frame or two later.** Reading a query back means
//! waiting for the GPU to reach it, and blocking for that would make the profiler
//! the most expensive thing in the frame — the classic way a measurement changes
//! what it measures. So a frame's results are collected whenever they happen to
//! be ready, and the panel shows the most recent complete set.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::device::Gpu;

/// How many marks one frame may take. Past this the extra are dropped rather
/// than wrapping onto the previous frame's slots, which would silently report
/// one pass's time against another's name.
const MAX_MARKS: u32 = 64;

/// One measured region of a frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub label: String,
    pub ms: f32,
}

pub struct GpuTimer {
    set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    read: wgpu::Buffer,
    /// Nanoseconds per timestamp tick, from the queue.
    period_ns: f32,
    next: u32,
    labels: Vec<String>,
    /// A readback is out; do not start another frame until it lands.
    inflight: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
    last: Vec<Span>,
    total_ms: f32,
}

impl GpuTimer {
    /// A timer, or `None` on a device without timestamp support.
    ///
    /// The feature is requested at device creation ([`Gpu`] asks for it when the
    /// adapter has it), so this answers "did that succeed" rather than
    /// re-checking the adapter — the two can disagree, and the device is the one
    /// that decides whether `write_timestamp` is legal.
    pub fn new(gpu: &Gpu) -> Option<Self> {
        let f = gpu.device.features();
        if !f.contains(wgpu::Features::TIMESTAMP_QUERY)
            || !f.contains(wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS)
        {
            return None;
        }
        let set = gpu.device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("gpu-timer"),
            ty: wgpu::QueryType::Timestamp,
            count: MAX_MARKS,
        });
        let bytes = (MAX_MARKS as u64) * 8;
        let resolve = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("gpu-timer-resolve"),
            size: bytes,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let read = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("gpu-timer-read"),
            size: bytes,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Some(Self {
            set,
            resolve,
            read,
            period_ns: gpu.queue.get_timestamp_period(),
            next: 0,
            labels: Vec::new(),
            inflight: Arc::new(AtomicBool::new(false)),
            ready: Arc::new(AtomicBool::new(false)),
            last: Vec::new(),
            total_ms: 0.0,
        })
    }

    /// Start a frame's marks. Answers `false` when the previous frame's readback
    /// has not landed — the caller then skips marking entirely, so a frame is
    /// either measured completely or not at all rather than in pieces.
    pub fn begin(&mut self) -> bool {
        if self.inflight.load(Ordering::Acquire) {
            return false;
        }
        self.next = 0;
        self.labels.clear();
        true
    }

    /// Mark the boundary between the previous region and one called `label`.
    ///
    /// The label names what comes AFTER the mark, so a frame reads as a sequence
    /// of marks and one [`end`](Self::end): region *i* runs from mark *i* to mark
    /// *i+1*.
    pub fn mark(&mut self, gpu: &Gpu, label: &str) {
        // One slot is always held back for the closing mark in `end`: without it
        // the final region has a start and no finish, and gets dropped — the
        // most expensive pass in a frame is often the last one.
        if self.next + 1 >= MAX_MARKS || self.inflight.load(Ordering::Acquire) {
            return;
        }
        let mut enc = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("gpu-timer") });
        enc.write_timestamp(&self.set, self.next);
        gpu.queue.submit([enc.finish()]);
        self.labels.push(label.to_string());
        self.next += 1;
    }

    /// Close the last region and ask for the frame's timings back.
    ///
    /// This writes the CLOSING timestamp — n labels need n+1 marks, and the
    /// unlabelled last one is it.
    pub fn end(&mut self, gpu: &Gpu) {
        // One label and its two bounding marks are the minimum that measure
        // anything.
        if self.next < 1 || self.inflight.load(Ordering::Acquire) {
            return;
        }
        let mut enc = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("gpu-timer") });
        enc.write_timestamp(&self.set, self.next);
        self.next += 1;
        let n = self.next;
        enc.resolve_query_set(&self.set, 0..n, &self.resolve, 0);
        enc.copy_buffer_to_buffer(&self.resolve, 0, &self.read, 0, (n as u64) * 8);
        gpu.queue.submit([enc.finish()]);

        self.inflight.store(true, Ordering::Release);
        let ready = self.ready.clone();
        let inflight = self.inflight.clone();
        self.read.slice(..(n as u64) * 8).map_async(wgpu::MapMode::Read, move |r| {
            if r.is_ok() {
                ready.store(true, Ordering::Release);
            } else {
                // The mapping failed — release the frame anyway, or the timer
                // stops reporting for good and reads as "this costs nothing".
                inflight.store(false, Ordering::Release);
            }
        });
    }

    /// Collect a landed readback, if there is one. Cheap and safe to call every
    /// frame; does not block.
    pub fn poll(&mut self) {
        if !self.ready.swap(false, Ordering::AcqRel) {
            return;
        }
        // n labels were closed by one extra mark — see `end`.
        let n = self.labels.len() as u64 + 1;
        {
            let view = self.read.slice(..n * 8).get_mapped_range();
            let ticks: Vec<u64> = view
                .chunks_exact(8)
                .map(|c| u64::from_le_bytes(c.try_into().unwrap_or([0; 8])))
                .collect();
            self.last.clear();
            let ms = |a: u64, b: u64| (b.saturating_sub(a) as f64 * self.period_ns as f64 / 1e6) as f32;
            for (i, label) in self.labels.iter().enumerate() {
                let (Some(&a), Some(&b)) = (ticks.get(i), ticks.get(i + 1)) else { break };
                self.last.push(Span { label: label.clone(), ms: ms(a, b) });
            }
            self.total_ms = match (ticks.first(), ticks.last()) {
                (Some(&a), Some(&b)) => ms(a, b),
                _ => 0.0,
            };
        }
        self.read.unmap();
        self.inflight.store(false, Ordering::Release);
    }

    /// The most recent complete frame's regions, in the order they ran.
    pub fn spans(&self) -> &[Span] {
        &self.last
    }

    /// That frame's total GPU time, first mark to last.
    pub fn total_ms(&self) -> f32 {
        self.total_ms
    }
}

#[cfg(test)]
mod tests {

    /// The label names the region that FOLLOWS the mark, so n marks describe
    /// n-1 regions. Off by one here and every cost is reported against the name
    /// of its neighbour — a profiler that lies is worse than no profiler, because
    /// its answer gets acted on.
    #[test]
    fn n_marks_describe_n_minus_one_regions() {
        let labels = ["prepass", "opaque", "post"];
        let ticks: [u64; 4] = [0, 100, 350, 400];
        let spans: Vec<(String, u64)> = (0..ticks.len() - 1)
            .map(|i| (labels[i].to_string(), ticks[i + 1] - ticks[i]))
            .collect();
        assert_eq!(spans.len(), labels.len());
        assert_eq!(spans[0], ("prepass".to_string(), 100));
        assert_eq!(spans[1], ("opaque".to_string(), 250));
        assert_eq!(spans[2], ("post".to_string(), 50));
        let total: u64 = spans.iter().map(|(_, t)| t).sum();
        assert_eq!(total, ticks[ticks.len() - 1] - ticks[0], "regions must tile the frame");
    }
}
