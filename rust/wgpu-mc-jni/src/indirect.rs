//! The indirect records a batched draw reads, and the cursor that keeps a submission's batches apart.
//!
//! `multi_draw_indexed_indirect` reads a run of fixed-size records out of one buffer - 20 bytes each,
//! the shape of `VkDrawIndexedIndirectCommand`, which wgpu keeps private and this side therefore
//! writes out by hand. What the JVM sends is a [`BatchRecord`], which is that record without the one
//! field a batch never varies: every draw of a batch is a single instance, so `instance_count` is
//! written here rather than travelled.
//!
//! **The cursor belongs to a submission, not to a frame.** A `Queue::write_buffer` is applied ahead of
//! every command of the submission that follows it, so two batches that wrote to the same bytes before
//! one submission would both read the second write. Each batch therefore takes its own slice of the
//! buffer, and the whole buffer is handed out again only where everything that could still read it has
//! been submitted - which is the one place `device` says a submission happened.
//!
//! A submission that runs out of room is not a broken frame: the batch is refused, and the caller draws
//! that run one draw at a time, which is the path a device with no real multi-draw takes anyway.
//! [`REFUSED`] counts those, and the first one is reported once, with the number to raise.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use wgpu_mc::wgpu;

/// One draw of a batch, as the JVM sends it: what varies between the draws of one call.
///
/// Everything that does not vary is in the draw call the batch is recorded with, which is why this is
/// four fields rather than a whole `DrawCall` per draw of the run.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct BatchRecord {
    /// First index, counting from the start of the bound index buffer.
    pub first: u32,
    /// How many indices this draw reads.
    pub count: u32,
    /// Added to every index, which is what lets one index buffer serve sections that sit at different
    /// offsets in one vertex arena - `glDrawElementsBaseVertex`'s `basevertex`, and an indirect
    /// record's `vertexOffset`.
    pub base_vertex: i32,
    /// Which record of the pass' per-draw storage this draw reads, for a shader that reads one.
    ///
    /// A multi-draw has exactly one channel for anything per draw - the record's `first_instance`,
    /// which reaches the vertex stage as `gl_InstanceIndex` - so a pipeline that needs per-draw data
    /// carries the index of that data here and reads it in the shader. Zero for a pipeline whose draws
    /// all read the same thing.
    pub instance: u32,
}

/// One indirect record, in the layout Vulkan and DX12 both read.
///
/// Written out field by field rather than taken from wgpu because wgpu's own record type is private to
/// it - the same reason [`BatchRecord`] above is spelled out rather than derived.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct IndirectCommand {
    pub index_count: u32,
    pub instance_count: u32,
    pub first_index: u32,
    pub base_vertex: i32,
    pub first_instance: u32,
}

/// How many bytes one record is, which is what a batched draw's offset is counted in.
pub const RECORD_SIZE: u64 = 20;

/// How many records the buffer holds: 320 KiB, several times a full render distance's worth of
/// sections. A submission that needs more than this falls back rather than growing, because the
/// fallback is slower and not wrong.
pub const CAPACITY: u32 = 16_384;

/// The buffer, and how much of it the submission in progress has written.
struct Scratch {
    buffer: wgpu::Buffer,
    used: u32,
}

static SCRATCH: Mutex<Option<Scratch>> = Mutex::new(None);

/// Batches refused for want of room, and whether that has been said.
///
/// Counted rather than logged each time: a frame over the limit would print one line per batch, which
/// is the frame's whole terrain.
static REFUSED: AtomicU64 = AtomicU64::new(0);
static REFUSED_REPORTED: AtomicBool = AtomicBool::new(false);

/// Draws carried by a batch since the counters were last read, for the render-stats line.
static BATCHED: AtomicU64 = AtomicU64::new(0);

/// Batches the native side declined to record as one call, for any reason but room.
///
/// **This is the number that says whether batching is happening at all**, which the draw count does
/// not: a declined batch is answered by drawing the run one draw at a time, so a frame where every
/// batch was declined looks exactly like a frame from before any of this existed. The first version of
/// the batched path had exactly that shape - the JVM handed over a stale binding combination, the
/// native side refused every run, and this number not being zero was the only thing that said so.
static DECLINED: AtomicU64 = AtomicU64::new(0);

/// Counts a batch the native side declined. See [`DECLINED`].
pub(crate) fn note_declined() {
    DECLINED.fetch_add(1, Ordering::Relaxed);
}

impl BatchRecord {
    /// The indirect record this becomes: one instance, and the per-draw number as `first_instance`.
    pub(crate) fn command(self) -> IndirectCommand {
        IndirectCommand {
            index_count: self.count,
            instance_count: 1,
            first_index: self.first,
            base_vertex: self.base_vertex,
            first_instance: self.instance,
        }
    }
}

/// Writes `commands` into this submission's slice of the record buffer, and answers where they are.
///
/// `None` means the batch cannot be recorded as one call: more draws than the buffer holds, or no room
/// left in this submission's slice of it. See [`REFUSED`].
pub(crate) fn write(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    commands: &[IndirectCommand],
) -> Option<(wgpu::Buffer, u64)> {
    let count = commands.len() as u32;

    if commands.is_empty() || count > CAPACITY {
        note_refused(count);
        return None;
    }

    let mut scratch = SCRATCH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if scratch.is_none() {
        *scratch = Some(Scratch {
            buffer: create(device),
            used: 0,
        });
    }

    let scratch = scratch.as_mut()?;

    if scratch.used + count > CAPACITY {
        note_refused(count);
        return None;
    }

    // Safety: `IndirectCommand` is `#[repr(C)]` of five four-byte fields with no padding, so the bytes
    // of the slice are exactly the records wgpu reads - and they are read from `commands`, which
    // outlives this call.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            commands.as_ptr() as *const u8,
            std::mem::size_of_val(commands),
        )
    };

    let offset = scratch.used as u64 * RECORD_SIZE;
    queue.write_buffer(&scratch.buffer, offset, bytes);
    scratch.used += count;

    BATCHED.fetch_add(count as u64, Ordering::Relaxed);

    Some((scratch.buffer.clone(), offset))
}

/// Starts a new submission's slice: everything recorded before it has been submitted, so the buffer may
/// be written from the front again.
///
/// Called where the encoder is submitted, which is the only place that is true - a cursor reset in the
/// middle of a frame would let a later batch overwrite records that an unsubmitted draw already names.
pub(crate) fn begin_submission() {
    if let Some(scratch) = SCRATCH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_mut()
    {
        scratch.used = 0;
    }
}

/// Reads and clears the two numbers the render-stats line reports: how many draws travelled in a batch,
/// and how many were turned away from one, since the last read.
///
/// Cleared on read for the same reason every other counter there is: the line describes a window, and a
/// total since startup says nothing about the frame that has just been drawn.
pub(crate) fn take_counts() -> (u64, u64) {
    (
        BATCHED.swap(0, Ordering::Relaxed),
        REFUSED.swap(0, Ordering::Relaxed) + DECLINED.swap(0, Ordering::Relaxed),
    )
}

fn create(device: &wgpu::Device) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("wgpu-mc batched draws"),
        size: CAPACITY as u64 * RECORD_SIZE,
        usage: wgpu::BufferUsages::INDIRECT | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn note_refused(count: u32) {
    REFUSED.fetch_add(count as u64, Ordering::Relaxed);

    if !REFUSED_REPORTED.swap(true, Ordering::Relaxed) {
        log::warn!(
            "wgpu-mc: a batch of {count} draw(s) did not fit the {CAPACITY} record(s) a submission has \
             room for, so it is drawn one draw at a time for the rest of this submission; raise \
             `indirect::CAPACITY` if that happens every frame"
        );
    }
}

#[cfg(test)]
mod batch_record_tests {
    use super::{BatchRecord, RECORD_SIZE};

    /// **The record the JVM writes and the record this side reads are the same sixteen bytes.**
    ///
    /// The JVM writes it by offset from `WmNative.BATCH_RECORD`, so a field added here without the
    /// Kotlin layout moving with it is a record whose fields are read out of the wrong ones - and the
    /// draw that comes out is a section drawn from another section's indices, which looks like a
    /// rendering bug rather than an ABI one.
    #[test]
    fn a_batch_record_is_four_four_byte_fields() {
        assert_eq!(std::mem::size_of::<BatchRecord>(), 16);
        assert_eq!(std::mem::offset_of!(BatchRecord, first), 0);
        assert_eq!(std::mem::offset_of!(BatchRecord, count), 4);
        assert_eq!(std::mem::offset_of!(BatchRecord, base_vertex), 8);
        assert_eq!(std::mem::offset_of!(BatchRecord, instance), 12);
    }

    /// The indirect record is 20 bytes, which is what the offset arithmetic and `RECORD_SIZE` assume.
    #[test]
    fn an_indirect_record_is_the_twenty_bytes_vulkan_reads() {
        assert_eq!(
            std::mem::size_of::<super::IndirectCommand>() as u64,
            RECORD_SIZE
        );
        assert_eq!(
            std::mem::offset_of!(super::IndirectCommand, first_instance),
            16
        );
    }

    /// A record becomes one instance whose number is the per-draw index, which is the whole per-draw
    /// channel a batched call has.
    #[test]
    fn a_record_becomes_one_instance_numbered_by_the_record() {
        let command = BatchRecord {
            first: 6,
            count: 12,
            base_vertex: -3,
            instance: 7,
        }
        .command();

        assert_eq!(command.index_count, 12);
        assert_eq!(command.instance_count, 1);
        assert_eq!(command.first_index, 6);
        assert_eq!(command.base_vertex, -3);
        assert_eq!(command.first_instance, 7);
    }
}
