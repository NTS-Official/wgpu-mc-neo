//! Frame timing from the GPU's own clock, through timestamp queries.
//!
//! Everything else this renderer reports is CPU time. What the GPU actually spent is only visible
//! through timestamp queries: `write_timestamp` records a marker into the command stream, the
//! driver answers with its own tick count, and the difference between two markers - multiplied by
//! `Queue::get_timestamp_period` - is how long the GPU took between them.
//!
//! A frame here is everything one presented frame submits, and the two markers are written into its
//! first and its last submission: the start goes into the fresh encoder the frame's first flush
//! leaves behind, the end into the blit that closes it. Both are on the same queue, so the
//! difference is that frame's GPU work and nothing else.
//!
//! Reading a result back means mapping a buffer the GPU wrote, which is only possible after the
//! submission that produced it has finished. That is why there is a ring of slots and the value of
//! frame *n* is picked up a couple of frames later, whenever the mapping completes - the render
//! thread never waits for the GPU here.
//!
//! Nothing in here runs unless the `gpu timestamps` debug switch is on: with it off, no timestamp
//! is written and no buffer exists.

use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use wgpu_mc::{WmRenderer, wgpu};

/// How many frames can be in flight between a frame's markers being submitted and its numbers
/// being read back. Three is what the mapping needs in practice: the submission has to complete,
/// which is a frame or two behind the recording.
const SLOTS: usize = 3;

/// How many bytes one frame's two timestamps take.
const PAIR_BYTES: u64 = 16;

/// How far apart the frames' resolve destinations are.
///
/// A resolve's destination offset has to be aligned to `QUERY_RESOLVE_BUFFER_ALIGNMENT` (256), which
/// a pair of timestamps is not: 16 bytes apart made wgpu reject the whole command buffer with
/// "Resolve buffer offset has to be aligned to QUERY_RESOLVE_BUFFER_ALIGNMENT" - and a validation
/// error on the render thread ends the process. Each frame therefore gets its own 256-byte slice of
/// the resolve buffer, of which the first 16 bytes are used.
const RESOLVE_STRIDE: u64 = wgpu::QUERY_RESOLVE_BUFFER_ALIGNMENT;

/// Whether the switch is on. Read on the recording path, so it is an atomic load and not a lock.
static ENABLED: AtomicBool = AtomicBool::new(false);

static TIMERS: Mutex<Option<GpuTimers>> = Mutex::new(None);

/// One frame's readback, and the mapping in flight for it.
struct Slot {
    /// `MAP_READ | COPY_DST`: where the resolved timestamps land for the CPU to read.
    readback: wgpu::Buffer,
    /// The mapping this frame's readback is waiting on, once its submission has been flushed.
    pending: Option<Receiver<Result<(), wgpu::BufferAsyncError>>>,
    /// Set when a frame's copy has been recorded and the mapping can be asked for.
    awaiting_map: bool,
}

struct GpuTimers {
    /// `SLOTS` pairs of timestamps, one pair per frame in flight.
    query_set: wgpu::QuerySet,
    /// `QUERY_RESOLVE | COPY_SRC`: queries cannot be copied straight to a mappable buffer.
    resolve: wgpu::Buffer,
    slots: Vec<Slot>,
    /// Nanoseconds per timestamp tick, which is what turns the driver's numbers into time.
    period_ns: f32,
    /// The slot the next frame will use, and the one the open frame is using.
    next_slot: usize,
    frame_slot: usize,
    /// Whether a frame has been started and not yet closed by its blit.
    frame_open: bool,
    frames: u64,
    total_ms: f64,
    last_ms: f64,
    max_ms: f64,
}

impl GpuTimers {
    fn new(wm: &WmRenderer) -> Option<GpuTimers> {
        let device = &wm.gpu.device;
        let required =
            wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS;

        if !device.features().contains(required) {
            log::error!(
                "wgpu-mc: gpu timestamps were asked for, but this device has no timestamp queries \
                 ({:?} is missing)",
                required - device.features()
            );
            return None;
        }

        let query_set = device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("wgpu-mc frame timestamps"),
            ty: wgpu::QueryType::Timestamp,
            count: (SLOTS * 2) as u32,
        });

        let resolve = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("wgpu-mc timestamp resolve"),
            size: SLOTS as u64 * RESOLVE_STRIDE,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });

        let slots = (0..SLOTS)
            .map(|_index| Slot {
                readback: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("wgpu-mc timestamp readback"),
                    size: PAIR_BYTES,
                    usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }),
                pending: None,
                awaiting_map: false,
            })
            .collect();

        log::info!(
            "wgpu-mc: gpu frame timestamps on, {:?} per tick",
            wm.gpu.queue.get_timestamp_period()
        );

        Some(GpuTimers {
            query_set,
            resolve,
            slots,
            period_ns: wm.gpu.queue.get_timestamp_period(),
            next_slot: 0,
            frame_slot: 0,
            frame_open: false,
            frames: 0,
            total_ms: 0.0,
            last_ms: 0.0,
            max_ms: 0.0,
        })
    }

    /// Takes whatever mappings have completed, and folds them into the statistics.
    fn collect(&mut self) {
        for slot in &mut self.slots {
            let outcome = slot.pending.as_ref().map(|pending| pending.try_recv());

            match outcome {
                Some(Ok(Ok(()))) => {
                    let data = slot
                        .readback
                        .slice(0..PAIR_BYTES)
                        .get_mapped_range()
                        .expect("the mapping reported done");
                    let start = u64::from_le_bytes(data[0..8].try_into().unwrap());
                    let end = u64::from_le_bytes(data[8..16].try_into().unwrap());
                    drop(data);

                    slot.readback.unmap();
                    slot.pending = None;

                    // The GPU's ticks, in nanoseconds, as a duration. `wrapping_sub` because a
                    // timestamp counter is free to wrap, and a wrap is still a difference.
                    let ms = end.wrapping_sub(start) as f64 * self.period_ns as f64 / 1e6;

                    self.last_ms = ms;
                    self.total_ms += ms;
                    self.max_ms = self.max_ms.max(ms);
                    self.frames += 1;
                }
                Some(Ok(Err(error))) => {
                    log::warn!("wgpu-mc: reading a frame's timestamps back failed: {error:?}");
                    slot.readback.unmap();
                    slot.pending = None;
                }
                Some(Err(TryRecvError::Disconnected)) => slot.pending = None,
                Some(Err(TryRecvError::Empty)) => {}
                None => {}
            }
        }
    }
}

/// Turns the measurement on or off, following the debug switch.
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);

    if enabled {
        return;
    }

    let mut guard = TIMERS.lock();
    let Some(timers) = guard.as_mut() else {
        return;
    };

    {
        // The numbers are not measured any more, so they stop being reported rather than staying
        // up as the last thing the switch saw.
        timers.frames = 0;
        timers.total_ms = 0.0;
        timers.max_ms = 0.0;
    }
}

/// Runs [body] against the timers, creating them on first use.
fn with_timers<R>(wm: &WmRenderer, body: impl FnOnce(&mut GpuTimers) -> R) -> Option<R> {
    if !ENABLED.load(Ordering::Relaxed) {
        return None;
    }

    let mut timers = TIMERS.lock();

    if timers.is_none() {
        *timers = GpuTimers::new(wm);
    }

    timers.as_mut().map(body)
}

/// Marks the start of a frame - the first submission after the last present.
///
/// Called with the encoder a flush has just installed, so the marker is the first command of the
/// frame's first submission.
pub fn frame_begin(wm: &WmRenderer, encoder: &mut wgpu::CommandEncoder) {
    with_timers(wm, |timers| {
        if timers.frame_open {
            return;
        }

        let slot = timers.next_slot;
        timers.next_slot = (slot + 1) % SLOTS;
        timers.frame_slot = slot;
        timers.frame_open = true;

        encoder.write_timestamp(&timers.query_set, (slot * 2) as u32);
    });
}

/// Marks the end of a frame - its last submission, the one that carries the present blit.
///
/// Records the end marker, resolves the pair and copies it where the CPU can reach it. All three
/// are commands in the submission that is about to be flushed, so the numbers exist as soon as that
/// submission has run.
pub fn frame_end(encoder: &mut wgpu::CommandEncoder, wm: &WmRenderer) {
    with_timers(wm, |timers| {
        if !timers.frame_open {
            return;
        }

        let slot = timers.frame_slot;
        let first = (slot * 2) as u32;
        let offset = slot as u64 * RESOLVE_STRIDE;

        encoder.write_timestamp(&timers.query_set, first + 1);
        encoder.resolve_query_set(&timers.query_set, first..first + 2, &timers.resolve, offset);
        encoder.copy_buffer_to_buffer(
            &timers.resolve,
            offset,
            &timers.slots[slot].readback,
            0,
            PAIR_BYTES,
        );

        timers.frame_open = false;
        timers.slots[slot].awaiting_map = true;
    });
}

/// Called once per presented frame: asks for the mappings a finished frame can now start, and
/// collects whatever has arrived.
pub fn frame_presented(wm: &WmRenderer) {
    let asked = with_timers(wm, |timers| {
        let mut asked = false;

        for index in 0..timers.slots.len() {
            if !timers.slots[index].awaiting_map {
                continue;
            }

            let slot = &mut timers.slots[index];
            slot.awaiting_map = false;

            // A mapping that never finished would leave the buffer mapped, and a mapped buffer
            // cannot be copied into - so anything still in flight is dropped first. It costs one
            // frame's number on a stall, not a validation error.
            if slot.pending.is_some() {
                slot.pending = None;
                slot.readback.unmap();
            }

            let (sender, receiver) = std::sync::mpsc::channel();
            slot.readback
                .slice(0..PAIR_BYTES)
                .map_async(wgpu::MapMode::Read, move |result| {
                    let _ = sender.send(result);
                });
            slot.pending = Some(receiver);
            asked = true;
        }

        asked
    });

    if asked != Some(true) {
        return;
    }

    // Running the mapping callbacks is what `poll` does; a `Poll` rather than a `Wait`, because the
    // render thread has a frame to present and the numbers can wait for it.
    if let Err(error) = wm.gpu.device.poll(wgpu::PollType::Poll) {
        log::warn!("wgpu-mc: polling for timestamp readbacks failed: {error:?}");
    }

    with_timers(wm, GpuTimers::collect);
}

/// Reports the frame times measured so far, from the render stats.
pub fn report() {
    let timers = TIMERS.lock();
    let Some(timers) = timers.as_ref() else {
        return;
    };

    if timers.frames == 0 {
        return;
    }

    log::info!(
        "wgpu-mc: gpu frame time: {:.2} ms average over {} frames (last {:.2} ms, worst {:.2} ms)",
        timers.total_ms / timers.frames as f64,
        timers.frames,
        timers.last_ms,
        timers.max_ms,
    );
}
