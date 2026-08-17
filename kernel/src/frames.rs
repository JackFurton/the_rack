//! The physical frame allocator: the bottom of the memory stack.
//!
//! Everything above this allocates through it. Page tables come from here in
//! #5, task stacks come from here in tier 2, DMA buffers in tier 4. A bug here
//! shows up as corruption somewhere else entirely, months later, so it is
//! worth being boring and checkable rather than clever.
//!
//! # Bitmap, not a free list
//!
//! The compact alternative is threading a linked list through the free frames
//! themselves, which costs no separate storage. It also means the allocator's
//! metadata lives in memory it has handed out the rights to, so a single wild
//! write into a freed frame corrupts the allocator rather than the caller, and
//! there is no way to ask "is this frame free" without walking the list.
//!
//! A bitmap costs one bit per frame. For the 256 MiB the `virt` machine gives
//! us that is 8 KiB of BSS, which buys O(1) queries, a memory map we can
//! print, and detection of double frees. Cheap at this scale.
//!
//! # Allocation order
//!
//! Always the lowest free frame, with no search hint. Slower in principle,
//! irrelevant at 1024 words of scanning, and deterministic, which means frame
//! reuse is reproducible and therefore testable. A smarter allocator can go in
//! later when something actually needs it.

use crate::sync::Lock;
use crate::{print, println};

/// Physical address where RAM starts on the QEMU `virt` machine.
pub const RAM_BASE: u64 = 0x4000_0000;

/// How much RAM we assume. Matches the `-m 256M` in the QEMU runner.
///
/// Hardcoded on purpose for now. Real discovery means parsing the device tree
/// blob QEMU leaves at `RAM_BASE`, which is tier 4 work. Until then this is a
/// number that must be kept in step with the runner, so the boot banner prints
/// it where a mismatch would be noticed.
pub const RAM_SIZE: u64 = 256 * 1024 * 1024;

/// Standard 4 KiB page. Also the granule the MMU will use in #5.
pub const FRAME_SIZE: u64 = 4096;

const FRAME_COUNT: usize = (RAM_SIZE / FRAME_SIZE) as usize;
const BITMAP_WORDS: usize = FRAME_COUNT / 64;

unsafe extern "C" {
    /// First byte past the kernel image, including its stack. From `linker.ld`.
    static __kernel_end: u8;
}

/// A 4 KiB physical frame, identified by its base address.
///
/// A newtype rather than a bare `u64` so a physical address cannot be passed
/// where a virtual one is wanted. That distinction costs nothing today, since
/// the MMU is off and the two are equal, and starts mattering enormously the
/// moment #5 turns paging on.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Frame(u64);

// Derived Debug prints the address in decimal, which is unreadable for a
// physical address and actively unhelpful in a panic message.
impl core::fmt::Debug for Frame {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Frame({:#012x})", self.0)
    }
}

impl Frame {
    /// Physical base address of this frame.
    pub fn addr(&self) -> u64 {
        self.0
    }

    /// Index of this frame within RAM.
    fn index(&self) -> usize {
        ((self.0 - RAM_BASE) / FRAME_SIZE) as usize
    }

    fn from_index(index: usize) -> Self {
        Frame(RAM_BASE + index as u64 * FRAME_SIZE)
    }

    /// Wrap a physical address that is already known to name a frame.
    pub fn from_addr(addr: u64) -> Self {
        debug_assert!(addr.is_multiple_of(FRAME_SIZE));
        Frame(addr)
    }

    /// Pointer to the frame's contents, through whichever alias currently
    /// works.
    ///
    /// Goes through `phys_to_virt` rather than casting the physical address
    /// directly, because once the kernel is running in the high half the
    /// physical address is no longer mapped at all.
    fn as_mut_ptr(&self) -> *mut u8 {
        crate::paging::phys_to_virt(self.0) as *mut u8
    }
}

struct Bitmap {
    /// One bit per frame. Set means allocated or reserved.
    bits: [u64; BITMAP_WORDS],
    free: usize,
    reserved: usize,
}

impl Bitmap {
    const fn new() -> Self {
        Self {
            bits: [0; BITMAP_WORDS],
            free: 0,
            reserved: 0,
        }
    }

    fn is_set(&self, index: usize) -> bool {
        self.bits[index / 64] & (1 << (index % 64)) != 0
    }

    fn set(&mut self, index: usize) {
        self.bits[index / 64] |= 1 << (index % 64);
    }

    fn clear(&mut self, index: usize) {
        self.bits[index / 64] &= !(1 << (index % 64));
    }

    /// Lowest index whose bit is clear.
    fn first_clear(&self) -> Option<usize> {
        for (word_index, word) in self.bits.iter().enumerate() {
            // All ones means every frame in this word is taken.
            if *word == u64::MAX {
                continue;
            }
            let bit = word.trailing_ones() as usize;
            return Some(word_index * 64 + bit);
        }
        None
    }
}

static ALLOCATOR: Lock<Bitmap> = Lock::new(Bitmap::new());

/// Claim everything from `RAM_BASE` up to the end of the kernel image, and
/// release the rest.
///
/// The reserved region covers two things that must never be handed out: the
/// device tree blob QEMU places at the base of RAM, and the kernel image
/// itself including the boot stack, which `linker.ld` places below
/// `__kernel_end`.
pub fn init() {
    let kernel_end = (&raw const __kernel_end) as u64;

    // Round up: a frame is only free if the whole frame is free.
    let first_free = kernel_end.div_ceil(FRAME_SIZE) * FRAME_SIZE;
    let reserved_frames = ((first_free - RAM_BASE) / FRAME_SIZE) as usize;

    let mut allocator = ALLOCATOR.lock();

    for index in 0..reserved_frames {
        allocator.set(index);
    }

    allocator.reserved = reserved_frames;
    allocator.free = FRAME_COUNT - reserved_frames;
}

/// Take the lowest free frame, zeroed.
///
/// Always zeroed, so nothing that was in a freed frame can leak into whatever
/// gets it next. Page tables in #5 depend on this: a table full of stale bytes
/// is a table full of garbage descriptors.
pub fn alloc() -> Option<Frame> {
    let mut allocator = ALLOCATOR.lock();

    let index = allocator.first_clear()?;
    allocator.set(index);
    allocator.free -= 1;
    drop(allocator);

    let frame = Frame::from_index(index);
    unsafe { core::ptr::write_bytes(frame.as_mut_ptr(), 0, FRAME_SIZE as usize) };

    Some(frame)
}

/// Take `count` physically consecutive frames, zeroed.
///
/// Needed for anything that has to be contiguous in physical memory rather
/// than merely contiguous in virtual memory: a kernel stack that we want to
/// reason about as one range, and eventually DMA buffers, where the device
/// does not go through our page tables at all.
///
/// Linear scan. At 65536 frames that is cheap and only happens at task
/// creation, and it stays honest about fragmentation rather than hiding it.
pub fn alloc_contiguous(count: usize) -> Option<Frame> {
    assert!(count > 0);

    let mut allocator = ALLOCATOR.lock();

    let mut run_start = 0;
    let mut run = 0;

    for index in 0..FRAME_COUNT {
        if allocator.is_set(index) {
            run = 0;
            continue;
        }

        if run == 0 {
            run_start = index;
        }
        run += 1;

        if run < count {
            continue;
        }

        for taken in run_start..run_start + count {
            allocator.set(taken);
        }
        allocator.free -= count;
        drop(allocator);

        let frame = Frame::from_index(run_start);
        unsafe {
            core::ptr::write_bytes(frame.as_mut_ptr(), 0, count * FRAME_SIZE as usize);
        }
        return Some(frame);
    }

    None
}

/// Hand back `count` frames starting at `first`.
pub fn free_contiguous(first: Frame, count: usize) {
    for index in 0..count {
        free(Frame::from_addr(first.addr() + index as u64 * FRAME_SIZE));
    }
}

/// Hand a frame back.
///
/// Panics on a frame outside RAM or one that is already free. Both mean a bug
/// upstream, and continuing quietly would let the same frame be handed to two
/// owners, which is the kind of fault that surfaces as unrelated corruption
/// much later.
pub fn free(frame: Frame) {
    assert!(
        frame.addr() >= RAM_BASE && frame.addr() < RAM_BASE + RAM_SIZE,
        "freeing a frame outside RAM"
    );
    assert!(
        frame.addr().is_multiple_of(FRAME_SIZE),
        "freeing a misaligned frame"
    );

    let index = frame.index();
    let mut allocator = ALLOCATOR.lock();

    assert!(allocator.is_set(index), "double free of frame {frame:?}");

    allocator.clear(index);
    allocator.free += 1;
}

/// Frames currently available.
pub fn free_frames() -> usize {
    ALLOCATOR.lock().free
}

/// Frames permanently held by the kernel image and the device tree blob.
pub fn reserved_frames() -> usize {
    ALLOCATOR.lock().reserved
}

/// Print what we decided the memory map looks like.
pub fn print_map() {
    let reserved = reserved_frames();
    let free = free_frames();
    let first_free = RAM_BASE + reserved as u64 * FRAME_SIZE;

    println!(
        "memory: {} MiB at {:#x}, {} frames of {} KiB",
        RAM_SIZE / 1024 / 1024,
        RAM_BASE,
        FRAME_COUNT,
        FRAME_SIZE / 1024
    );
    println!(
        "  reserved : {:#012x}..{:#012x}  {:>4} KiB  kernel image and DTB",
        RAM_BASE,
        first_free,
        reserved as u64 * FRAME_SIZE / 1024
    );
    println!(
        "  free     : {:#012x}..{:#012x}  {:>4} MiB  {} frames",
        first_free,
        RAM_BASE + RAM_SIZE,
        free as u64 * FRAME_SIZE / 1024 / 1024,
        free
    );
}

/// Exercise the allocator's actual guarantees, not just that it returns
/// something.
///
/// The property worth checking is that a reused frame comes back zeroed. An
/// allocator that only zeroes on first use passes every naive test and quietly
/// leaks the previous owner's data forever after.
pub fn self_test() {
    let before = free_frames();

    let a = alloc().expect("allocator had no frames");
    let b = alloc().expect("allocator had no frames");
    assert_ne!(
        a.addr(),
        b.addr(),
        "allocator handed out the same frame twice"
    );
    assert_eq!(free_frames(), before - 2);

    // Fresh frames must be zeroed.
    assert!(is_zeroed(a), "freshly allocated frame was not zeroed");

    // Scribble, hand it back, and take it again. Allocation is lowest-first
    // and deterministic, so the same frame comes back and the zeroing is
    // genuinely exercised rather than coincidentally observed.
    unsafe { core::ptr::write_bytes(a.as_mut_ptr(), 0xab, FRAME_SIZE as usize) };
    assert!(!is_zeroed(a), "scribble did not take");

    free(a);
    free(b);
    assert_eq!(
        free_frames(),
        before,
        "frames were not returned to the pool"
    );

    let reused = alloc().expect("allocator had no frames");
    assert_eq!(reused.addr(), a.addr(), "expected the lowest frame back");
    assert!(
        is_zeroed(reused),
        "reused frame still held the previous owner's data"
    );
    free(reused);

    assert_eq!(free_frames(), before);

    print!("frame self test: passed, ");
    println!(
        "reuse of {:#x} came back clean, {} frames free",
        a.addr(),
        before
    );
}

fn is_zeroed(frame: Frame) -> bool {
    let bytes = unsafe { core::slice::from_raw_parts(frame.as_mut_ptr(), FRAME_SIZE as usize) };
    bytes.iter().all(|byte| *byte == 0)
}
