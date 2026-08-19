//! The split virtqueue: how a driver and a device pass buffers to each other.
//!
//! Three pieces of shared memory, and the split is by who writes what. The
//! descriptor table says where the buffers are, and the driver writes it. The
//! available ring says which descriptors are ready, and the driver writes it.
//! The used ring says which ones are finished, and the *device* writes it. No
//! field has two writers, which is what makes the whole thing work without a
//! lock across a boundary where no lock could exist.
//!
//! # Everything the device sees is a physical address
//!
//! The device does not walk our page tables. It has no idea that the kernel
//! lives in the high half or that a task's memory is remapped per address
//! space. Every address handed to it, including the addresses of the rings
//! themselves, is physical, and every address the kernel uses to touch that
//! same memory goes through the physical map. Getting this backwards does not
//! fault: the device happily reads whatever is at the address it was given.
//!
//! # Ordering
//!
//! The descriptors have to be visible before the index that publishes them,
//! and the index before the notification. On paper that needs barriers, and
//! they are here. In practice QEMU's device only looks when it is notified,
//! and the emulated memory is coherent with the guest's, so removing them
//! changes nothing observable on this machine. They are written for the
//! machine where it does matter, and the boot log cannot prove they work.
//!
//! Cache maintenance is a separate question and deliberately absent. The
//! device tree marks these transports `dma-coherent`, so the device sees the
//! same memory the caches do. A non-coherent device would need the rings
//! cleaned and invalidated by hand, which is a different and much less
//! forgiving piece of code.

use core::sync::atomic::{Ordering, fence};

use crate::frames::{self, Frame};
use crate::paging;
use crate::virtio::Transport;

/// How many descriptors a queue holds.
///
/// Power of two, because the indices are free running `u16` that wrap and the
/// ring position is taken modulo the size. Sixteen is enough for a queue depth
/// of one with room to chain, and small enough that all three rings fit in a
/// single frame.
pub const QUEUE_SIZE: u16 = 16;

const DESCRIPTOR_BYTES: u64 = 16;

// Offsets within the one frame the rings live in. The specification wants the
// descriptor table 16 byte aligned, the available ring 2, and the used ring 4;
// these are all further apart than that, which costs a little space and makes
// the layout obvious.
const DESC_OFFSET: u64 = 0;
const AVAIL_OFFSET: u64 = 256;
const USED_OFFSET: u64 = 512;

/// The descriptor continues into another one.
const DESC_NEXT: u16 = 1;
/// The device writes this buffer. Without it the buffer is read only to the
/// device, and a device asked to fill a read only buffer refuses the whole
/// request rather than half completing it.
const DESC_WRITE: u16 = 2;

// Transport registers used to point a device at its queue.
const QUEUE_SEL: usize = 0x030;
const QUEUE_NUM_MAX: usize = 0x034;
const QUEUE_NUM: usize = 0x038;
const QUEUE_READY: usize = 0x044;
const QUEUE_NOTIFY: usize = 0x050;
const QUEUE_DESC_LOW: usize = 0x080;
const QUEUE_DESC_HIGH: usize = 0x084;
const QUEUE_DRIVER_LOW: usize = 0x090;
const QUEUE_DRIVER_HIGH: usize = 0x094;
const QUEUE_DEVICE_LOW: usize = 0x0a0;
const QUEUE_DEVICE_HIGH: usize = 0x0a4;

/// One buffer in a request.
#[derive(Clone, Copy)]
pub struct Buffer {
    /// Physical address. Not a pointer, because it is not ours to dereference
    /// and not the device's to translate.
    pub pa: u64,
    pub len: u32,
    /// True if the device fills it, false if the device reads it.
    pub device_writes: bool,
}

/// Why a request could not be queued.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The device's queue is smaller than the one we insist on.
    TooSmall(u32),
    /// No memory for the rings.
    NoMemory,
    /// Not enough free descriptors for the chain.
    Full,
    /// A chain of no buffers is not a request.
    Empty,
}

/// A queue, and the driver's half of its bookkeeping.
pub struct Queue {
    frame: Frame,
    size: u16,
    /// Head of the free descriptor list, threaded through the `next` fields of
    /// the descriptors themselves. The device never looks at a free
    /// descriptor, so its `next` is ours to use as a list pointer.
    free_head: u16,
    free_count: u16,
    /// How far we have consumed the used ring. The device's index runs ahead
    /// of this and both wrap.
    last_used: u16,
}

impl Queue {
    /// Build queue `index` on `transport` and tell the device where it is.
    pub fn new(transport: &Transport, index: u32) -> Result<Queue, Error> {
        transport.write(QUEUE_SEL, index);

        let max = transport.read(QUEUE_NUM_MAX);
        if max < QUEUE_SIZE as u32 {
            return Err(Error::TooSmall(max));
        }

        // One frame, zeroed by the allocator, which matters: the available
        // ring's index starts at zero and the device believes it.
        let frame = frames::alloc().ok_or(Error::NoMemory)?;
        let pa = frame.addr();

        let queue = Queue {
            frame,
            size: QUEUE_SIZE,
            free_head: 0,
            free_count: QUEUE_SIZE,
            last_used: 0,
        };

        // Thread the free list through the descriptors.
        for index in 0..QUEUE_SIZE {
            queue.write_descriptor_next(index, index + 1);
        }

        transport.write(QUEUE_NUM, QUEUE_SIZE as u32);
        transport.write(QUEUE_DESC_LOW, (pa + DESC_OFFSET) as u32);
        transport.write(QUEUE_DESC_HIGH, ((pa + DESC_OFFSET) >> 32) as u32);
        transport.write(QUEUE_DRIVER_LOW, (pa + AVAIL_OFFSET) as u32);
        transport.write(QUEUE_DRIVER_HIGH, ((pa + AVAIL_OFFSET) >> 32) as u32);
        transport.write(QUEUE_DEVICE_LOW, (pa + USED_OFFSET) as u32);
        transport.write(QUEUE_DEVICE_HIGH, ((pa + USED_OFFSET) >> 32) as u32);

        // Everything the device is about to read has to be in memory before it
        // is told the queue is live.
        fence(Ordering::SeqCst);
        transport.write(QUEUE_READY, 1);

        Ok(queue)
    }

    fn ring_ptr<T>(&self, offset: u64) -> *mut T {
        paging::phys_to_virt(self.frame.addr() + offset) as *mut T
    }

    fn write_descriptor(&self, index: u16, buffer: &Buffer, next: u16, chained: bool) {
        let base = DESC_OFFSET + index as u64 * DESCRIPTOR_BYTES;
        let flags =
            if chained { DESC_NEXT } else { 0 } | if buffer.device_writes { DESC_WRITE } else { 0 };

        unsafe {
            self.ring_ptr::<u64>(base).write_volatile(buffer.pa);
            self.ring_ptr::<u32>(base + 8).write_volatile(buffer.len);
            self.ring_ptr::<u16>(base + 12).write_volatile(flags);
            self.ring_ptr::<u16>(base + 14).write_volatile(next);
        }
    }

    fn write_descriptor_next(&self, index: u16, next: u16) {
        let base = DESC_OFFSET + index as u64 * DESCRIPTOR_BYTES;
        unsafe { self.ring_ptr::<u16>(base + 14).write_volatile(next) };
    }

    fn descriptor_next(&self, index: u16) -> u16 {
        let base = DESC_OFFSET + index as u64 * DESCRIPTOR_BYTES;
        unsafe { self.ring_ptr::<u16>(base + 14).read_volatile() }
    }

    fn available_index(&self) -> u16 {
        unsafe { self.ring_ptr::<u16>(AVAIL_OFFSET + 2).read_volatile() }
    }

    fn used_index(&self) -> u16 {
        unsafe { self.ring_ptr::<u16>(USED_OFFSET + 2).read_volatile() }
    }

    /// Queue a chain of buffers and return the descriptor that heads it.
    ///
    /// The head is the identity of the request: it is what comes back in the
    /// used ring, and it is how the driver knows which of several outstanding
    /// requests just finished.
    pub fn add(&mut self, buffers: &[Buffer]) -> Result<u16, Error> {
        if buffers.is_empty() {
            return Err(Error::Empty);
        }
        if buffers.len() > self.free_count as usize {
            return Err(Error::Full);
        }

        let head = self.free_head;
        let mut index = head;

        for (position, buffer) in buffers.iter().enumerate() {
            let next = self.descriptor_next(index);
            let chained = position + 1 < buffers.len();
            self.write_descriptor(index, buffer, next, chained);

            if chained {
                index = next;
            } else {
                self.free_head = next;
            }
        }

        self.free_count -= buffers.len() as u16;

        // The ring holds descriptor ids, and its position wraps with the
        // index. The index itself does not wrap at the ring size: it is a free
        // running counter, and taking it modulo the size is the reader's job.
        let position = self.available_index() % self.size;
        unsafe {
            self.ring_ptr::<u16>(AVAIL_OFFSET + 4 + position as u64 * 2)
                .write_volatile(head)
        };

        // The descriptors and the ring entry must be visible before the index
        // that publishes them. A device that sees the new index and the old
        // descriptor reads a buffer that was never filled in.
        fence(Ordering::SeqCst);
        unsafe {
            self.ring_ptr::<u16>(AVAIL_OFFSET + 2)
                .write_volatile(self.available_index().wrapping_add(1))
        };
        fence(Ordering::SeqCst);

        Ok(head)
    }

    /// Tell the device there is something to do.
    pub fn notify(&self, transport: &Transport, index: u32) {
        transport.write(QUEUE_NOTIFY, index);
    }

    /// Take one finished request, if the device has finished any.
    ///
    /// Returns the head descriptor and how many bytes the device wrote.
    pub fn take_used(&mut self) -> Option<(u16, u32)> {
        if self.last_used == self.used_index() {
            return None;
        }

        // Nothing the device wrote may be read before the index that announced
        // it, which is the mirror of the ordering on the way in.
        fence(Ordering::SeqCst);

        let position = (self.last_used % self.size) as u64;
        let entry = USED_OFFSET + 4 + position * 8;
        let head = unsafe { self.ring_ptr::<u32>(entry).read_volatile() } as u16;
        let written = unsafe { self.ring_ptr::<u32>(entry + 4).read_volatile() };

        self.last_used = self.last_used.wrapping_add(1);
        self.recycle(head);

        Some((head, written))
    }

    /// Put a finished chain back on the free list.
    fn recycle(&mut self, head: u16) {
        let mut index = head;
        let mut length = 1;

        // Walk to the end of the chain, which is the descriptor whose flags no
        // longer say NEXT.
        while self.descriptor_flags(index) & DESC_NEXT != 0 {
            index = self.descriptor_next(index);
            length += 1;
        }

        self.write_descriptor_next(index, self.free_head);
        self.free_head = head;
        self.free_count += length;
    }

    fn descriptor_flags(&self, index: u16) -> u16 {
        let base = DESC_OFFSET + index as u64 * DESCRIPTOR_BYTES;
        unsafe { self.ring_ptr::<u16>(base + 12).read_volatile() }
    }

    /// How many descriptors are available.
    pub fn free(&self) -> u16 {
        self.free_count
    }

    /// Stop the queue and give its memory back.
    pub fn release(self, transport: &Transport, index: u32) {
        transport.write(QUEUE_SEL, index);
        transport.write(QUEUE_READY, 0);
        fence(Ordering::SeqCst);
        frames::free(self.frame);
    }
}

/// A queue, driven end to end against a real device.
///
/// The device is the entropy source, deliberately: it is the simplest thing in
/// the specification. One buffer in, random bytes out, no header to build and
/// no status byte to interpret. Everything that goes wrong here is the ring's
/// fault, which is what makes it a test of the ring.
pub fn self_test() {
    crate::print!("virtqueue self test: ");

    let transport =
        crate::virtio::find(crate::virtio::DEVICE_ENTROPY).expect("no entropy device attached");

    transport
        .negotiate(0)
        .expect("entropy device would not agree on features");

    let mut queue = Queue::new(&transport, 0).expect("could not build queue 0");
    assert_eq!(queue.free(), QUEUE_SIZE);

    // Only now. The device may start reading the queue the moment this is set.
    transport.set_driver_ok();

    // A frame the device will write into. Zeroed by the allocator, which is
    // what makes "did the device write anything" answerable.
    let buffer = frames::alloc().expect("no memory for a buffer");
    let sample = 16;

    let head = queue
        .add(&[Buffer {
            pa: buffer.addr(),
            len: sample,
            device_writes: true,
        }])
        .expect("could not queue the request");
    assert_eq!(
        head, 0,
        "the first request should take the first descriptor"
    );
    assert_eq!(queue.free(), QUEUE_SIZE - 1);

    queue.notify(&transport, 0);

    // Polled, not waited on. The interrupt path exists but the driver that
    // uses it is a task, and that task does not exist until #46.
    let mut spins = 0;
    let finished = loop {
        if let Some(finished) = queue.take_used() {
            break finished;
        }
        spins += 1;
        assert!(spins < 10_000_000, "the device never answered");
        core::hint::spin_loop();
    };

    let (used_head, written) = finished;
    assert_eq!(used_head, head, "a different request came back");
    assert!(written > 0 && written <= sample, "wrote {written} bytes");

    // The descriptor is back on the free list, which is what makes a second
    // request possible at all.
    assert_eq!(queue.free(), QUEUE_SIZE);

    // And the bytes actually arrived. Checked through the physical map,
    // because the address the device was given was physical.
    let filled = paging::phys_to_virt(buffer.addr()) as *const u8;
    let mut nonzero = 0;
    for offset in 0..written as usize {
        if unsafe { filled.add(offset).read_volatile() } != 0 {
            nonzero += 1;
        }
    }
    assert!(
        nonzero > 0,
        "the device reported {written} bytes and wrote none"
    );

    transport.ack_interrupt();
    queue.release(&transport, 0);
    transport.reset();
    frames::free(buffer);

    crate::println!("passed, {written} bytes arrived by DMA, {nonzero} of them not zero");
}
