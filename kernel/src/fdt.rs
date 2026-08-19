//! The flattened device tree the firmware leaves in memory for us.
//!
//! Every question this kernel currently answers with a constant (where RAM
//! is, where the UART is, where the GIC is) is really a question about the
//! machine, and on a machine we did not build. The device tree is the answer
//! the firmware already wrote down. This module is the smallest part of
//! reading it: find the blob, decide whether it is a blob at all, and hold on
//! to it.
//!
//! Parsing the tree itself is #41. This is only the header.
//!
//! # Why the pointer is an argument and not a static
//!
//! The address arrives in x0 at the reset vector and is gone one instruction
//! later, because the first thing `boot.S` does is read `mpidr_el1` into the
//! same register. Saving it into a static would be the obvious fix and is
//! wrong twice over: the BSS has not been zeroed yet at that point, so the
//! store would be undone a few instructions later, and once the BSS *is*
//! zeroed the MMU is still off, so a static is being written through an
//! address that will mean something different in a moment. Carrying the value
//! in a register and passing it as an argument sidesteps both. It is only a
//! `u64`, and it costs nothing to hand along.
//!
//! # Why the header is checked before it is believed
//!
//! The blob is the one piece of input this kernel takes from outside itself.
//! It is a structure full of offsets and lengths, sitting at an address we
//! were merely told about, and every field in it is a chance to read somewhere
//! we should not. On QEMU it is always well formed, which is exactly why the
//! checking has to be written now: the first machine that hands us a bad one
//! will not be the one we are testing on.

use crate::frames::{RAM_BASE, RAM_SIZE};
use crate::paging;
use crate::sync::Lock;
use crate::{print, println};

/// Big endian 0xd00dfeed at the front of every flattened device tree.
pub const MAGIC: u32 = 0xd00d_feed;

/// Ten big endian `u32` fields, and the spec has not added an eleventh.
const HEADER_SIZE: u32 = 40;

/// Version 17 is the current format and the only one QEMU emits. Anything
/// older lacks `size_dt_struct`, so a v16 blob cannot be walked with the same
/// code, and a blob that claims to *need* something newer than 17 is telling
/// us it has structure we would misread.
const SUPPORTED_VERSION: u32 = 17;

/// A sanity bound, not a spec limit. QEMU's `virt` blob is a few kilobytes;
/// anything past a megabyte is a corrupt length rather than a device tree, and
/// refusing it early keeps a garbage `totalsize` from being used as a range.
const MAX_BLOB: u32 = 1024 * 1024;

/// Why a candidate blob was refused.
///
/// Each variant carries the value that failed, because "the device tree looked
/// wrong" is not a debuggable message on a machine with no debugger.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The firmware passed a null pointer, or nothing at all.
    Null,
    /// The blob must be 8 byte aligned. The spec says so, and every offset in
    /// it is computed assuming it.
    Misaligned(u64),
    /// First four bytes were not `MAGIC`.
    BadMagic(u32),
    /// `totalsize` cannot even hold the header it is part of.
    TooSmall(u32),
    /// `totalsize` is past what could plausibly be a device tree.
    TooLarge(u32),
    /// The format version is one we cannot walk.
    Version(u32),
    /// A block's offset and size run off the end of the blob.
    BlockOutOfRange,
    /// The blob is not entirely inside RAM.
    OutsideRam(u64),
}

impl Error {
    /// One line, for a console with no formatter for enums.
    pub fn describe(self) -> &'static str {
        match self {
            Error::Null => "null pointer",
            Error::Misaligned(_) => "not 8 byte aligned",
            Error::BadMagic(_) => "bad magic",
            Error::TooSmall(_) => "totalsize too small",
            Error::TooLarge(_) => "totalsize too large",
            Error::Version(_) => "unsupported version",
            Error::BlockOutOfRange => "block runs off the end",
            Error::OutsideRam(_) => "blob is not in RAM",
        }
    }
}

/// The device tree header, byte swapped into native order once so nothing
/// downstream has to remember that the file format is big endian.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Header {
    pub totalsize: u32,
    pub off_dt_struct: u32,
    pub off_dt_strings: u32,
    pub off_mem_rsvmap: u32,
    pub version: u32,
    pub last_comp_version: u32,
    pub boot_cpuid_phys: u32,
    pub size_dt_strings: u32,
    pub size_dt_struct: u32,
}

/// A device tree that has passed its header checks.
///
/// Holds the physical address, because that is what the firmware gave us and
/// what the rest of the kernel talks about, and a virtual one to actually read
/// through. Today they differ by the physical map offset; keeping both means
/// #41 never has to guess which kind of address it is holding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Blob {
    pub pa: u64,
    pub va: u64,
    pub header: Header,
}

static BLOB: Lock<Option<Blob>> = Lock::new(None);

/// Read a big endian `u32` from `va + offset`.
///
/// # Safety
///
/// `va + offset` must be readable. Callers below only use this on the first 40
/// bytes, which `probe` has already established are inside the mapping it was
/// handed.
unsafe fn be32(va: u64, offset: u32) -> u32 {
    let ptr = (va + offset as u64) as *const u32;
    u32::from_be(unsafe { ptr.read() })
}

/// Decide whether the bytes at `va` are a device tree we can walk.
///
/// Takes a virtual address rather than a physical one so the self test can
/// point it at a buffer on the stack. Nothing here dereferences past the
/// header until `totalsize` has been checked, and nothing at all is
/// dereferenced until the magic matches.
///
/// # Safety
///
/// At least 40 bytes at `va` must be readable.
pub unsafe fn probe(va: u64) -> Result<Header, Error> {
    if va == 0 {
        return Err(Error::Null);
    }
    if !va.is_multiple_of(8) {
        return Err(Error::Misaligned(va));
    }

    let magic = unsafe { be32(va, 0) };
    if magic != MAGIC {
        return Err(Error::BadMagic(magic));
    }

    let totalsize = unsafe { be32(va, 4) };
    if totalsize < HEADER_SIZE {
        return Err(Error::TooSmall(totalsize));
    }
    if totalsize > MAX_BLOB {
        return Err(Error::TooLarge(totalsize));
    }

    let header = Header {
        totalsize,
        off_dt_struct: unsafe { be32(va, 8) },
        off_dt_strings: unsafe { be32(va, 12) },
        off_mem_rsvmap: unsafe { be32(va, 16) },
        version: unsafe { be32(va, 20) },
        last_comp_version: unsafe { be32(va, 24) },
        boot_cpuid_phys: unsafe { be32(va, 28) },
        size_dt_strings: unsafe { be32(va, 32) },
        size_dt_struct: unsafe { be32(va, 36) },
    };

    // `last_comp_version` is the blob saying how old a reader it will still
    // work with. If that is newer than us, the blob is telling us plainly that
    // we will misread it, and the right answer is to believe it.
    if header.last_comp_version > SUPPORTED_VERSION || header.version < header.last_comp_version {
        return Err(Error::Version(header.version));
    }

    // Every offset in the header is an index into the blob, and the blob is
    // only `totalsize` long. Checked arithmetic throughout: an offset near
    // `u32::MAX` plus a size is exactly how a bounds check gets skipped.
    let ends_inside = |offset: u32, size: u32| match offset.checked_add(size) {
        Some(end) => end <= totalsize,
        None => false,
    };

    if !ends_inside(header.off_dt_struct, header.size_dt_struct)
        || !ends_inside(header.off_dt_strings, header.size_dt_strings)
        || header.off_mem_rsvmap >= totalsize
    {
        return Err(Error::BlockOutOfRange);
    }

    Ok(header)
}

/// Take the pointer the firmware passed and record the blob it points at.
///
/// Runs in the high half, so the physical address is reached through the
/// physical map like any other. Returns the error rather than panicking:
/// booting without a device tree is survivable today, since everything it
/// would tell us is still hardcoded, and a kernel that dies before it can say
/// why is worse than one that carries on and complains.
pub fn init(pa: u64) -> Result<Blob, Error> {
    if pa == 0 {
        return Err(Error::Null);
    }

    // Bound the read before making it. `probe` is safe to call only on memory
    // that is mapped, and the physical map covers RAM and nothing else.
    if pa < RAM_BASE || pa.saturating_add(HEADER_SIZE as u64) > RAM_BASE + RAM_SIZE {
        return Err(Error::OutsideRam(pa));
    }

    let va = paging::phys_to_virt(pa);
    let header = unsafe { probe(va) }?;

    if pa + header.totalsize as u64 > RAM_BASE + RAM_SIZE {
        return Err(Error::OutsideRam(pa));
    }

    let blob = Blob { pa, va, header };
    *BLOB.lock() = Some(blob);
    Ok(blob)
}

/// The device tree, if there is one.
pub fn blob() -> Option<Blob> {
    *BLOB.lock()
}

/// What the firmware told us, for the boot banner.
pub fn print_info(blob: &Blob) {
    println!("device tree:");
    println!("  address       : {:#018x} physical", blob.pa);
    println!(
        "  total size    : {} bytes ({:#x})",
        blob.header.totalsize, blob.header.totalsize
    );
    println!(
        "  version       : {} (readable back to {})",
        blob.header.version, blob.header.last_comp_version
    );
    println!(
        "  struct block  : {:#x} + {:#x}",
        blob.header.off_dt_struct, blob.header.size_dt_struct
    );
    println!(
        "  strings block : {:#x} + {:#x}",
        blob.header.off_dt_strings, blob.header.size_dt_strings
    );
    println!("  boot cpu      : {}", blob.header.boot_cpuid_phys);
}

/// A device tree header built by hand, so the checks can be aimed at something
/// deliberately broken.
///
/// `repr(align(8))` because one of the things under test is the alignment
/// check, and a buffer that landed on an odd address by luck would fail for
/// the wrong reason.
#[repr(align(8))]
struct Scratch([u8; 64]);

impl Scratch {
    /// A well formed v17 header describing an otherwise empty 64 byte blob.
    fn good() -> Self {
        let mut scratch = Scratch([0; 64]);
        scratch.put(0, MAGIC);
        scratch.put(4, 64); // totalsize
        scratch.put(8, 40); // off_dt_struct
        scratch.put(12, 56); // off_dt_strings
        scratch.put(16, 40); // off_mem_rsvmap
        scratch.put(20, 17); // version
        scratch.put(24, 16); // last_comp_version
        scratch.put(28, 0); // boot_cpuid_phys
        scratch.put(32, 8); // size_dt_strings
        scratch.put(36, 16); // size_dt_struct
        scratch
    }

    fn put(&mut self, offset: usize, value: u32) {
        self.0[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }

    fn probe(&self) -> Result<Header, Error> {
        unsafe { probe(self.0.as_ptr() as u64) }
    }
}

/// Every refusal in `probe`, aimed at a header built to trip it.
///
/// The point of the test is not that a good blob parses; QEMU proves that at
/// every boot. It is that a bad one is refused rather than walked, and the
/// only way to see that is to write the bad ones down.
pub fn self_test() {
    print!("fdt self test: ");

    let good = Scratch::good();
    let header = good.probe().expect("a well formed header must parse");
    assert_eq!(header.totalsize, 64);
    assert_eq!(header.version, 17);
    assert_eq!(header.size_dt_struct, 16);

    // A null pointer, which is what a firmware that passes nothing looks like.
    assert_eq!(unsafe { probe(0) }, Err(Error::Null));

    // Misaligned. Deliberately checked before anything is read, so an odd
    // address never becomes an unaligned `u32` load.
    let address = good.0.as_ptr() as u64;
    assert_eq!(
        unsafe { probe(address + 4) },
        Err(Error::Misaligned(address + 4))
    );

    // Not a device tree at all. This is the case that matters most: without
    // it, a stale or wild pointer is walked as though it were structure.
    let mut wrong = Scratch::good();
    wrong.put(0, 0xdead_beef);
    assert_eq!(wrong.probe(), Err(Error::BadMagic(0xdead_beef)));

    // A size that cannot hold the header it is part of.
    let mut small = Scratch::good();
    small.put(4, HEADER_SIZE - 1);
    assert_eq!(small.probe(), Err(Error::TooSmall(HEADER_SIZE - 1)));

    // A size that is not a size. Left unchecked this is the number that later
    // becomes the bound on every other read.
    let mut large = Scratch::good();
    large.put(4, MAX_BLOB + 1);
    assert_eq!(large.probe(), Err(Error::TooLarge(MAX_BLOB + 1)));

    // A blob that says it needs a newer reader than we are.
    let mut future = Scratch::good();
    future.put(24, SUPPORTED_VERSION + 1);
    assert_eq!(future.probe(), Err(Error::Version(17)));

    // The struct block claims to extend past the end of the blob. Believed,
    // this is a read of 4 GiB of whatever follows the device tree.
    let mut overrun = Scratch::good();
    overrun.put(36, 4096);
    assert_eq!(overrun.probe(), Err(Error::BlockOutOfRange));

    // The same overrun expressed as an offset that wraps. `off + size <= total`
    // is true here in wrapping arithmetic and false in reality.
    let mut wrap = Scratch::good();
    wrap.put(8, u32::MAX);
    wrap.put(36, 64);
    assert_eq!(wrap.probe(), Err(Error::BlockOutOfRange));

    // The strings block gets the same treatment as the struct block, because
    // it is read with the same kind of arithmetic.
    let mut strings = Scratch::good();
    strings.put(12, 60);
    strings.put(32, 8);
    assert_eq!(strings.probe(), Err(Error::BlockOutOfRange));

    // A blob outside RAM, which is what a plausible looking but stale pointer
    // looks like. Refused before it is dereferenced, since the physical map
    // does not cover it and reading it would fault in the kernel.
    assert_eq!(init(0x1000).unwrap_err(), Error::OutsideRam(0x1000));

    println!("passed, a good header parses and ten broken ones are refused");
}
