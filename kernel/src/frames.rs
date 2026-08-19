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

use core::sync::atomic::{AtomicU64, Ordering};

use crate::sync::Lock;
use crate::{print, println};

/// Where RAM starts if nothing tells us otherwise. True of the QEMU `virt`
/// machine, and the only guess available when there is no device tree.
pub const DEFAULT_RAM_BASE: u64 = 0x4000_0000;

/// How much RAM to assume when nothing tells us otherwise.
pub const DEFAULT_RAM_SIZE: u64 = 256 * 1024 * 1024;

/// Standard 4 KiB page. Also the granule the MMU uses.
pub const FRAME_SIZE: u64 = 4096;

/// The most RAM this kernel can track.
///
/// The bitmap is a fixed array, so something has to bound it. A kernel with an
/// allocator would size the bitmap from what the device tree reported and
/// place it in the memory it describes; this one has no allocator until the
/// bitmap exists, which is the loop that makes bootstrapping allocators
/// awkward. 4 GiB of tracking costs 128 KiB of BSS, which is a fair price for
/// not having to solve that yet. RAM past this point is ignored, loudly.
pub const MAX_RAM: u64 = 4 * 1024 * 1024 * 1024;

const MAX_FRAMES: usize = (MAX_RAM / FRAME_SIZE) as usize;
const BITMAP_WORDS: usize = MAX_FRAMES / 64;

/// Where RAM is, as discovered at boot.
///
/// Plain atomics rather than a `Lock`, because these are read on the
/// allocation path and written once, before there is anything to race with.
/// Relaxed loads and stores of a `u64` are ordinary `ldr` and `str` on this
/// architecture, which also makes them safe to touch before the MMU is on,
/// where an exclusive access would be on questionable ground.
static RAM_BASE_ADDR: AtomicU64 = AtomicU64::new(DEFAULT_RAM_BASE);
static RAM_LENGTH: AtomicU64 = AtomicU64::new(DEFAULT_RAM_SIZE);

/// First address past the kernel image, remembered rather than recomputed.
///
/// The reserved frames used to be exactly the kernel image, so the boundary
/// could be derived from a count. Reservations are scattered now (the device
/// tree is 128 MiB up on this machine), and deriving a boundary from a count
/// draws a picture of memory that is not the one the bitmap holds.
static IMAGE_END: AtomicU64 = AtomicU64::new(0);

/// Physical address where RAM starts.
pub fn ram_base() -> u64 {
    RAM_BASE_ADDR.load(Ordering::Relaxed)
}

/// How much RAM there is, as the machine described it.
pub fn ram_size() -> u64 {
    RAM_LENGTH.load(Ordering::Relaxed)
}

/// How many frames that works out to.
pub fn frame_count() -> usize {
    (ram_size() / FRAME_SIZE) as usize
}

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
        ((self.0 - ram_base()) / FRAME_SIZE) as usize
    }

    fn from_index(index: usize) -> Self {
        Frame(ram_base() + index as u64 * FRAME_SIZE)
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

    /// Mark every frame in `range` taken, a word at a time.
    ///
    /// Bit by bit would be correct and unusably slow in the one place this is
    /// needed: `init` runs with the MMU off, where every access is
    /// uncached device memory, and the tail of an unpopulated bitmap is close
    /// to a million frames. A machine that appeared to hang on boot turned out
    /// to be that loop.
    fn set_range(&mut self, range: core::ops::Range<usize>) {
        for index in range.clone().take_while(|index| !index.is_multiple_of(64)) {
            self.set(index);
        }

        let first_whole_word = range.start.next_multiple_of(64);
        if first_whole_word >= range.end {
            return;
        }

        let words = first_whole_word / 64..range.end / 64;
        self.bits[words].fill(u64::MAX);

        for index in range.end / 64 * 64..range.end {
            self.set(index);
        }
    }

    /// The inverse of `set_range`, and word wise for the same reason.
    fn clear_range(&mut self, range: core::ops::Range<usize>) {
        for index in range.clone().take_while(|index| !index.is_multiple_of(64)) {
            self.clear(index);
        }

        let first_whole_word = range.start.next_multiple_of(64);
        if first_whole_word >= range.end {
            return;
        }

        let words = first_whole_word / 64..range.end / 64;
        self.bits[words].fill(0);

        for index in range.end / 64 * 64..range.end {
            self.clear(index);
        }
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
/// That covers the kernel image including the boot stack, which `linker.ld`
/// places below `__kernel_end`. It does not cover the device tree blob: QEMU
/// puts that 128 MiB into RAM, nowhere near the image, and it is reserved
/// separately once the header has been read and its real size is known.
pub fn init(base: u64, size: u64) {
    // Truncation rather than refusal, so a machine with more RAM than the
    // bitmap can track still boots on the part we can describe.
    let size = size.min(MAX_RAM) / FRAME_SIZE * FRAME_SIZE;

    RAM_BASE_ADDR.store(base, Ordering::Relaxed);
    RAM_LENGTH.store(size, Ordering::Relaxed);

    let frames = (size / FRAME_SIZE) as usize;
    let kernel_end = (&raw const __kernel_end) as u64;

    // Round up: a frame is only free if the whole frame is free.
    let first_free = kernel_end.div_ceil(FRAME_SIZE) * FRAME_SIZE;
    let reserved_frames = ((first_free - base) / FRAME_SIZE) as usize;

    let mut allocator = ALLOCATOR.lock();

    allocator.set_range(0..reserved_frames);

    // Everything past the end of RAM is marked taken rather than bounds
    // checked on every allocation. The bitmap is sized for the largest machine
    // we can track and this one is smaller, so the frames that do not exist
    // have to look occupied or the lowest-first search will walk straight off
    // the end of memory and hand out an address nothing answers to.
    allocator.set_range(frames..MAX_FRAMES);

    allocator.reserved = reserved_frames;
    allocator.free = frames - reserved_frames;

    IMAGE_END.store(first_free, Ordering::Relaxed);
}

/// Why a range could not be reserved.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReserveError {
    /// The range is not entirely inside RAM.
    OutsideRam(u64),
    /// A frame in the range has already been handed out, so reserving it now
    /// would mean two owners.
    AlreadyTaken(u64),
}

/// Claim a range of physical memory the allocator must never hand out.
///
/// Needed for memory that belongs to somebody else and happens to sit in RAM:
/// the device tree blob, and later whatever `/reserved-memory` describes. The
/// kernel image is not reserved this way, because `init` already has it.
///
/// Checks the whole range before setting a single bit. A partial reservation
/// would be worse than none: the caller sees a failure and assumes nothing
/// happened, while some of the range has quietly left the pool for good.
pub fn reserve_range(base: u64, len: u64) -> Result<usize, ReserveError> {
    if len == 0 {
        return Ok(0);
    }

    // Round outwards. A frame is only safe to hand out if none of it is
    // spoken for, so a range covering one byte of a frame reserves the frame.
    let start = base / FRAME_SIZE * FRAME_SIZE;
    let end = base
        .checked_add(len)
        .ok_or(ReserveError::OutsideRam(base))?
        .div_ceil(FRAME_SIZE)
        * FRAME_SIZE;

    if start < ram_base() || end > ram_base() + ram_size() {
        return Err(ReserveError::OutsideRam(base));
    }

    let first = ((start - ram_base()) / FRAME_SIZE) as usize;
    let count = ((end - start) / FRAME_SIZE) as usize;

    let mut allocator = ALLOCATOR.lock();

    for index in first..first + count {
        if allocator.is_set(index) {
            return Err(ReserveError::AlreadyTaken(
                ram_base() + index as u64 * FRAME_SIZE,
            ));
        }
    }

    for index in first..first + count {
        allocator.set(index);
    }
    allocator.reserved += count;
    allocator.free -= count;

    Ok(count)
}

/// Is this address in a frame that is allocated or reserved?
pub fn is_taken(addr: u64) -> bool {
    if !(ram_base()..ram_base() + ram_size()).contains(&addr) {
        return false;
    }
    ALLOCATOR
        .lock()
        .is_set(((addr - ram_base()) / FRAME_SIZE) as usize)
}

/// Take on the real memory map, once the device tree has been read.
///
/// The allocator has to exist before the tree can be read (page tables are
/// allocated, and reading the tree needs the MMU), so it starts on a guess and
/// is corrected here. Growing means the frames past the guess stop being
/// marked "does not exist"; shrinking means they start being.
///
/// Refuses to move RAM out from under itself. The base address is baked into
/// every page table built so far and into every frame handed out, so a tree
/// that disagrees about where RAM starts is a machine this boot path cannot
/// serve. Saying so beats carrying on with two answers.
pub fn adopt(base: u64, size: u64) -> Result<i64, ReserveError> {
    if base != ram_base() {
        return Err(ReserveError::OutsideRam(base));
    }

    let size = size.min(MAX_RAM) / FRAME_SIZE * FRAME_SIZE;
    let old = frame_count();
    let new = (size / FRAME_SIZE) as usize;

    let mut allocator = ALLOCATOR.lock();

    if new > old {
        allocator.clear_range(old..new);
        allocator.free += new - old;
    } else {
        // Shrinking can only take frames nobody has. Allocation is lowest
        // first and this is the top of memory, so in practice nothing is up
        // there; checking anyway, because "in practice" is how a frame ends up
        // owned by two things.
        for index in new..old {
            if allocator.is_set(index) {
                return Err(ReserveError::AlreadyTaken(
                    ram_base() + index as u64 * FRAME_SIZE,
                ));
            }
        }
        allocator.set_range(new..old);
        allocator.free -= old - new;
    }

    RAM_LENGTH.store(size, Ordering::Relaxed);
    Ok(new as i64 - old as i64)
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

    for index in 0..frame_count() {
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
        frame.addr() >= ram_base() && frame.addr() < ram_base() + ram_size(),
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

/// Frames permanently held: the kernel image, and anything `reserve_range`
/// has taken out of the pool since.
pub fn reserved_frames() -> usize {
    ALLOCATOR.lock().reserved
}

/// Print what we decided the memory map looks like.
pub fn print_map() {
    let reserved = reserved_frames();
    let free = free_frames();
    let image_end = IMAGE_END.load(Ordering::Relaxed);

    println!(
        "memory: {} MiB at {:#x}, {} frames of {} KiB",
        ram_size() / 1024 / 1024,
        ram_base(),
        frame_count(),
        FRAME_SIZE / 1024
    );
    println!(
        "  kernel   : {:#012x}..{:#012x}  {:>4} KiB",
        ram_base(),
        image_end,
        (image_end - ram_base()) / 1024
    );
    println!(
        "  reserved : {:>6} frames  {:>4} KiB  image and anything the machine claimed",
        reserved,
        reserved as u64 * FRAME_SIZE / 1024
    );
    println!(
        "  free     : {:>6} frames  {:>4} MiB",
        free,
        free as u64 * FRAME_SIZE / 1024 / 1024
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

    // Reserving a range that includes a frame already handed out has to fail,
    // and has to fail without taking the rest of the range with it. The range
    // below starts on a free frame and ends on an allocated one, so a
    // reservation that set bits as it went would leave the first frame gone
    // and report an error at the same time.
    let low = alloc().expect("allocator had no frames");
    let high = alloc().expect("allocator had no frames");
    free(low);

    let free_before = free_frames();
    let reserved_before = reserved_frames();
    assert_eq!(
        reserve_range(low.addr(), FRAME_SIZE * 2),
        Err(ReserveError::AlreadyTaken(high.addr())),
        "reserving over an allocated frame must be refused"
    );
    assert_eq!(
        free_frames(),
        free_before,
        "a refused reservation took frames anyway"
    );
    assert_eq!(reserved_frames(), reserved_before);
    assert!(!is_taken(low.addr()), "a refused reservation kept a frame");

    free(high);

    // The top of RAM has to be reachable through the physical map. The map is
    // built before the device tree can be read, so it covers whatever the
    // kernel guessed at, and a machine with more memory than that has frames
    // the allocator will hand out and the kernel cannot touch. Nothing else in
    // the boot path allocates high enough to notice.
    let last = ram_base() + ram_size() - FRAME_SIZE;
    assert!(!is_taken(last), "the top of RAM is spoken for");
    let ptr = crate::paging::phys_to_virt(last) as *mut u64;
    unsafe {
        ptr.write_volatile(0x7261_636b);
        assert_eq!(
            ptr.read_volatile(),
            0x7261_636b,
            "the last frame in RAM is not mapped"
        );
        ptr.write_volatile(0);
    }

    // Moving RAM out from under the allocator is refused. Every page table
    // built so far and every frame handed out is relative to the old base, so
    // a tree that disagrees is a machine this boot path cannot serve.
    assert_eq!(
        adopt(ram_base() + FRAME_SIZE, ram_size()),
        Err(ReserveError::OutsideRam(ram_base() + FRAME_SIZE))
    );

    // A range outside RAM is a bad address, not an empty reservation.
    assert_eq!(
        reserve_range(0x1000, FRAME_SIZE),
        Err(ReserveError::OutsideRam(0x1000))
    );

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
