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

// -------------------------------------------------------------------------
// The structure block.
//
// A depth first serialisation of the tree, made of four byte tokens. A node
// opens with FDT_BEGIN_NODE and its name, carries its properties, then its
// children, then FDT_END_NODE. Everything is big endian and everything is four
// byte aligned, including the strings that are not.
//
// The awkward part of the format is that a property's name is not stored with
// it. It is an offset into a separate strings block, so reading one property
// means two bounds checked reads in two different parts of the blob.
// -------------------------------------------------------------------------

const FDT_BEGIN_NODE: u32 = 0x1;
const FDT_END_NODE: u32 = 0x2;
const FDT_PROP: u32 = 0x3;
const FDT_NOP: u32 = 0x4;
const FDT_END: u32 = 0x9;

/// How deep the walker will follow the tree.
///
/// A fixed array rather than a stack, because the walker is holding one entry
/// per level and this kernel has no allocator to grow into. Real trees are
/// four or five deep; sixteen is room to spare, and a blob that claims to be
/// deeper is treated as malformed rather than allowed to write past the array.
const MAX_DEPTH: usize = 16;

/// What `#address-cells` and `#size-cells` mean when a node does not say.
///
/// These are the spec's defaults and they are not what `virt` uses, which is
/// exactly why they must not be guessed. Inheriting the parent's values would
/// also be wrong: the property is not inherited, it is defaulted.
const DEFAULT_ADDRESS_CELLS: u32 = 2;
const DEFAULT_SIZE_CELLS: u32 = 1;

/// A node found in the tree.
///
/// `address_cells` and `size_cells` are the *parent's*, because that is what
/// this node's `reg` is written in. A node's own `#address-cells` describes its
/// children, never itself, and getting that backwards is the classic way to
/// decode a `reg` into nonsense that happens to look plausible.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Node {
    /// Offset of the `FDT_BEGIN_NODE` token from the start of the blob.
    pub offset: u32,
    /// 1 for the root, 2 for its children, and so on.
    pub depth: usize,
    pub address_cells: u32,
    pub size_cells: u32,
}

/// One thing the walker found.
pub enum Step {
    /// A node opened. The walker's depth is now this node's depth.
    Node(Node, &'static str),
    /// A property of whichever node is currently open.
    Property(&'static str, &'static [u8]),
}

impl Blob {
    /// Read a big endian `u32` at `offset`, if it is inside the blob.
    fn u32_at(&self, offset: u32) -> Option<u32> {
        if offset.checked_add(4)? > self.header.totalsize {
            return None;
        }
        Some(unsafe { be32(self.va, offset) })
    }

    /// Borrow `len` bytes at `offset`, if they are inside the blob.
    ///
    /// The `'static` is honest: the blob is reserved out of the frame
    /// allocator for the life of the machine, so nothing can take it back.
    fn bytes_at(&self, offset: u32, len: u32) -> Option<&'static [u8]> {
        if offset.checked_add(len)? > self.header.totalsize {
            return None;
        }
        let ptr = (self.va + offset as u64) as *const u8;
        Some(unsafe { core::slice::from_raw_parts(ptr, len as usize) })
    }

    /// Read a NUL terminated string at `offset`, refusing to run past `limit`.
    ///
    /// The limit is the point of this function. A string in a device tree is
    /// terminated by a byte the blob supplies, which means a blob that forgets
    /// the terminator is asking us to read until we find one somewhere else.
    fn str_at(&self, offset: u32, limit: u32) -> Option<&'static str> {
        let limit = limit.min(self.header.totalsize);
        if offset >= limit {
            return None;
        }

        let mut end = offset;
        while end < limit {
            if unsafe { ((self.va + end as u64) as *const u8).read() } == 0 {
                let bytes = self.bytes_at(offset, end - offset)?;
                return core::str::from_utf8(bytes).ok();
            }
            end += 1;
        }
        None
    }

    /// Resolve a property name, which lives in the strings block.
    fn string(&self, name_offset: u32) -> Option<&'static str> {
        let start = self.header.off_dt_strings.checked_add(name_offset)?;
        let end = self
            .header
            .off_dt_strings
            .checked_add(self.header.size_dt_strings)?;
        if start >= end {
            return None;
        }
        self.str_at(start, end)
    }

    /// Walk from the top of the structure block.
    pub fn walk(&self) -> Walker {
        Walker::at(self, self.header.off_dt_struct)
    }

    /// Every property of a node, in order.
    pub fn properties(&self, node: &Node) -> Properties {
        // A node's properties always come before its children, so they start
        // immediately after the name and end at the first token that is not
        // another property. The format guarantees the ordering, which is what
        // makes this a straight run rather than a search.
        let mut offset = node.offset + 4;
        offset = match self.str_at(offset, self.header.totalsize) {
            Some(name) => align4(offset + name.len() as u32 + 1),
            None => u32::MAX,
        };

        Properties {
            blob: *self,
            offset,
        }
    }

    /// One property of a node by name.
    pub fn property(&self, node: &Node, name: &str) -> Option<&'static [u8]> {
        self.properties(node)
            .find(|(found, _)| *found == name)
            .map(|(_, value)| value)
    }

    /// A property that is a single big endian `u32`.
    pub fn property_u32(&self, node: &Node, name: &str) -> Option<u32> {
        let value = self.property(node, name)?;
        let bytes: [u8; 4] = value.get(..4)?.try_into().ok()?;
        Some(u32::from_be_bytes(bytes))
    }

    /// A property that is a NUL terminated string.
    pub fn property_str(&self, node: &Node, name: &str) -> Option<&'static str> {
        let value = self.property(node, name)?;
        let end = value.iter().position(|byte| *byte == 0)?;
        core::str::from_utf8(&value[..end]).ok()
    }

    /// Entry `index` of a node's `reg`, as an address and a length.
    ///
    /// Decoded with the parent's cell counts, which is the whole reason `Node`
    /// carries them. On `virt` they are 2 and 2, so a hardcoded pair of `u64`
    /// reads would work here and break on the first machine that uses 1 and 1,
    /// which is most 32 bit ones.
    pub fn reg(&self, node: &Node, index: usize) -> Option<(u64, u64)> {
        // More than two cells is a 128 bit address, which we could not hold
        // and have no way to honour. Refusing beats truncating.
        if node.address_cells > 2 || node.size_cells > 2 || node.address_cells == 0 {
            return None;
        }

        let value = self.property(node, "reg")?;
        let entry = ((node.address_cells + node.size_cells) * 4) as usize;
        let start = index.checked_mul(entry)?;
        let bytes = value.get(start..start.checked_add(entry)?)?;

        let (address, rest) = take_cells(bytes, node.address_cells)?;
        let (size, _) = take_cells(rest, node.size_cells)?;
        Some((address, size))
    }

    /// Find a node by path, for example `/` or `/memory` or `/soc/uart`.
    ///
    /// A component matches a node whose name is equal to it, or whose name is
    /// it followed by `@` and a unit address. That elision is what lets the
    /// caller ask for `/memory` without knowing in advance that this machine
    /// calls it `memory@40000000`.
    pub fn find_node(&self, path: &str) -> Option<Node> {
        let wanted = path.split('/').filter(|part| !part.is_empty()).count();

        // `on_path[d]` answers "is the node currently open at depth d the one
        // the path asked for". Depth 0 is outside the root and trivially true,
        // which is what makes the root itself a normal case rather than one.
        let mut on_path = [false; MAX_DEPTH];
        on_path[0] = true;

        let mut walker = self.walk();
        while let Some(step) = walker.next() {
            let Step::Node(node, name) = step else {
                continue;
            };

            let matched = if node.depth == 1 {
                // The root's name is empty, and every path starts there.
                name.is_empty()
            } else {
                let component = path
                    .split('/')
                    .filter(|part| !part.is_empty())
                    .nth(node.depth - 2);
                match component {
                    Some(component) => on_path[node.depth - 1] && name_matches(name, component),
                    None => false,
                }
            };

            on_path[node.depth] = matched;

            if matched && node.depth == wanted + 1 {
                return Some(node);
            }
        }

        None
    }

    /// The first node after `offset` whose `compatible` list contains `wanted`.
    ///
    /// Takes an offset rather than returning an iterator so a caller can ask
    /// for the next one, which is how #44 will find 32 virtio transports
    /// without the walker having to be borrowed across the loop.
    pub fn find_compatible_after(&self, wanted: &str, offset: u32) -> Option<Node> {
        let mut walker = self.walk();
        while let Some(step) = walker.next() {
            let Step::Node(node, _) = step else { continue };
            if node.offset <= offset {
                continue;
            }
            if let Some(list) = self.property(&node, "compatible")
                && string_list_contains(list, wanted)
            {
                return Some(node);
            }
        }
        None
    }

    /// The first node whose `compatible` list contains `wanted`.
    pub fn find_compatible(&self, wanted: &str) -> Option<Node> {
        self.find_compatible_after(wanted, 0)
    }
}

/// The properties of one node, in the order the blob stores them.
pub struct Properties {
    blob: Blob,
    offset: u32,
}

impl Iterator for Properties {
    type Item = (&'static str, &'static [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let token = self.blob.u32_at(self.offset)?;

            // A NOP is a token that was deleted in place by something that
            // edited the blob without wanting to move everything after it.
            if token == FDT_NOP {
                self.offset += 4;
                continue;
            }

            if token != FDT_PROP {
                return None;
            }

            let len = self.blob.u32_at(self.offset + 4)?;
            let name_offset = self.blob.u32_at(self.offset + 8)?;
            let value = self.blob.bytes_at(self.offset + 12, len)?;
            let name = self.blob.string(name_offset)?;

            self.offset = align4(self.offset + 12 + len);
            return Some((name, value));
        }
    }
}

/// A cursor over the structure block.
///
/// Holds the cell counts for every level currently open, because a node's
/// `reg` is decoded with numbers that were declared by its parent, several
/// tokens ago and possibly several levels up.
pub struct Walker {
    blob: Blob,
    offset: u32,
    end: u32,
    depth: usize,
    /// `cells[d]` is what the node open at depth `d` declared for its
    /// children. `cells[0]` is the spec default, for the root itself.
    cells: [(u32, u32); MAX_DEPTH],
}

impl Walker {
    fn at(blob: &Blob, offset: u32) -> Self {
        let end = blob
            .header
            .off_dt_struct
            .saturating_add(blob.header.size_dt_struct);

        Walker {
            blob: *blob,
            offset,
            end,
            depth: 0,
            cells: [(DEFAULT_ADDRESS_CELLS, DEFAULT_SIZE_CELLS); MAX_DEPTH],
        }
    }

    /// How deep the walk currently is. 0 is outside the root.
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// The next node or property, or `None` at the end of the tree.
    ///
    /// `None` also covers every malformed case: a token that is not a token, a
    /// length that runs off the end, a name with no terminator, a tree deeper
    /// than the walker can hold. Stopping is the only safe answer to all of
    /// them, and distinguishing "ended" from "gave up" would tempt a caller
    /// into carrying on with half a tree.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<Step> {
        loop {
            if self.offset >= self.end {
                return None;
            }

            let token = self.blob.u32_at(self.offset)?;

            match token {
                FDT_NOP => self.offset += 4,

                FDT_END => return None,

                FDT_BEGIN_NODE => {
                    let offset = self.offset;
                    let name = self.blob.str_at(offset + 4, self.end)?;

                    if self.depth + 1 >= MAX_DEPTH {
                        return None;
                    }

                    self.depth += 1;
                    let parent = self.cells[self.depth - 1];
                    self.cells[self.depth] = (DEFAULT_ADDRESS_CELLS, DEFAULT_SIZE_CELLS);

                    // Name, its NUL, then padding back to a four byte boundary.
                    self.offset = align4(offset + 4 + name.len() as u32 + 1);

                    return Some(Step::Node(
                        Node {
                            offset,
                            depth: self.depth,
                            address_cells: parent.0,
                            size_cells: parent.1,
                        },
                        name,
                    ));
                }

                FDT_END_NODE => {
                    self.depth = self.depth.checked_sub(1)?;
                    self.offset += 4;
                }

                FDT_PROP => {
                    let len = self.blob.u32_at(self.offset + 4)?;
                    let name_offset = self.blob.u32_at(self.offset + 8)?;
                    let value = self.blob.bytes_at(self.offset + 12, len)?;
                    let name = self.blob.string(name_offset)?;

                    self.offset = align4(self.offset + 12 + len);

                    // These two are what every `reg` in the subtree below is
                    // read with, so they are recorded as they go past rather
                    // than looked up later.
                    if self.depth > 0
                        && let Some(cells) = read_cells(value)
                    {
                        if name == "#address-cells" {
                            self.cells[self.depth].0 = cells;
                        } else if name == "#size-cells" {
                            self.cells[self.depth].1 = cells;
                        }
                    }

                    return Some(Step::Property(name, value));
                }

                // Not a token at all, which means we are no longer reading
                // structure. Everything after this point is guesswork.
                _ => return None,
            }
        }
    }
}

/// Round up to the next four byte boundary, saturating rather than wrapping.
fn align4(offset: u32) -> u32 {
    offset.saturating_add(3) & !3
}

/// Read `count` big endian cells as one number, and return what is left.
fn take_cells(bytes: &[u8], count: u32) -> Option<(u64, &[u8])> {
    let mut value: u64 = 0;
    let mut rest = bytes;

    for _ in 0..count {
        let (cell, tail) = rest.split_at_checked(4)?;
        value = (value << 32) | u32::from_be_bytes(cell.try_into().ok()?) as u64;
        rest = tail;
    }

    Some((value, rest))
}

/// A `#address-cells` or `#size-cells` value, if it is one.
fn read_cells(value: &[u8]) -> Option<u32> {
    let bytes: [u8; 4] = value.get(..4)?.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

/// Does this node name match this path component?
///
/// `memory@40000000` matches `memory`, because the part after `@` is the unit
/// address and a caller asking for a path by name should not have to know it.
fn name_matches(name: &str, component: &str) -> bool {
    if name == component {
        return true;
    }
    match name.split_once('@') {
        Some((base, _)) => base == component,
        None => false,
    }
}

/// Is `wanted` one of the NUL separated strings in this property?
///
/// `compatible` is a list, most specific first, and matching it as one string
/// would miss every device whose node names a specific model before the
/// generic one it is compatible with.
pub fn string_list_contains(list: &[u8], wanted: &str) -> bool {
    list.split(|byte| *byte == 0)
        .any(|entry| entry == wanted.as_bytes())
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

// -------------------------------------------------------------------------
// Walking the tree, tested against trees built to be wrong.
// -------------------------------------------------------------------------

/// The strings block every handcrafted test tree shares. Property names are
/// offsets into this, which is how the format stores them.
const TEST_STRINGS: &[u8] = b"compatible\0reg\0#address-cells\0#size-cells\0";
const STR_COMPATIBLE: u32 = 0;
const STR_REG: u32 = 11;
const STR_ADDRESS_CELLS: u32 = 15;
const STR_SIZE_CELLS: u32 = 30;

/// A device tree assembled a token at a time.
///
/// Building the malformed cases by hand is the only way to test them: QEMU
/// will never hand us a truncated blob, and the day something does is the day
/// this code is running somewhere it cannot be debugged.
#[repr(align(8))]
struct Tree {
    bytes: [u8; 512],
    len: usize,
}

impl Tree {
    fn new() -> Self {
        Tree {
            bytes: [0; 512],
            len: 40,
        }
    }

    fn u32(&mut self, value: u32) {
        self.bytes[self.len..self.len + 4].copy_from_slice(&value.to_be_bytes());
        self.len += 4;
    }

    fn begin(&mut self, name: &str) {
        self.u32(FDT_BEGIN_NODE);
        self.bytes[self.len..self.len + name.len()].copy_from_slice(name.as_bytes());
        self.len += name.len() + 1;
        self.len = self.len.next_multiple_of(4);
    }

    fn end_node(&mut self) {
        self.u32(FDT_END_NODE);
    }

    fn prop(&mut self, name_offset: u32, value: &[u8]) {
        self.u32(FDT_PROP);
        self.u32(value.len() as u32);
        self.u32(name_offset);
        self.bytes[self.len..self.len + value.len()].copy_from_slice(value);
        self.len += value.len();
        self.len = self.len.next_multiple_of(4);
    }

    fn cells(&mut self, address: u32, size: u32) {
        self.prop(STR_ADDRESS_CELLS, &address.to_be_bytes());
        self.prop(STR_SIZE_CELLS, &size.to_be_bytes());
    }

    /// A property whose declared length is a lie.
    fn lying_prop(&mut self, name_offset: u32, claimed: u32) {
        self.u32(FDT_PROP);
        self.u32(claimed);
        self.u32(name_offset);
    }

    /// Write the header and hand back a blob, checked exactly the way the
    /// firmware's own would be.
    fn finish(&mut self) -> Blob {
        self.u32(FDT_END);

        let struct_start = 40u32;
        let struct_size = self.len as u32 - struct_start;
        let strings_start = self.len as u32;
        self.bytes[self.len..self.len + TEST_STRINGS.len()].copy_from_slice(TEST_STRINGS);
        self.len += TEST_STRINGS.len();

        let mut header = [0u32; 10];
        header[0] = MAGIC;
        header[1] = self.len as u32;
        header[2] = struct_start;
        header[3] = strings_start;
        header[4] = struct_start;
        header[5] = 17;
        header[6] = 16;
        header[7] = 0;
        header[8] = TEST_STRINGS.len() as u32;
        header[9] = struct_size;

        for (index, field) in header.iter().enumerate() {
            self.bytes[index * 4..index * 4 + 4].copy_from_slice(&field.to_be_bytes());
        }

        let va = self.bytes.as_ptr() as u64;
        Blob {
            pa: 0,
            va,
            header: unsafe { probe(va) }.expect("the test tree must pass the header checks"),
        }
    }
}

/// A tree with a root and one child, in whichever cell counts are asked for.
fn test_tree(address_cells: u32, size_cells: u32) -> Tree {
    let mut tree = Tree::new();
    tree.begin("");
    tree.cells(address_cells, size_cells);
    tree.prop(STR_COMPATIBLE, b"test,root\0");
    tree.begin("memory@1000");
    tree.prop(STR_COMPATIBLE, b"test,memory\0test,thing\0");
    tree.prop(
        STR_REG,
        &[
            0, 0, 0, 0, 0, 0, 0x10, 0, // 0x1000 as two cells
            0, 0, 0, 0, 0, 0, 0x20, 0, // 0x2000 as two cells
        ],
    );
    tree.end_node();
    tree.end_node();
    tree
}

/// Walking, on trees built to be walked and on trees built to trip it.
pub fn tree_self_test() {
    print!("device tree self test: ");

    // A path resolves, and its `reg` is decoded with the cells the parent
    // declared.
    let tree = test_tree(2, 2).finish();
    let memory = tree.find_node("/memory").expect("no /memory node");
    assert_eq!(memory.depth, 2);
    assert_eq!(tree.reg(&memory, 0), Some((0x1000, 0x2000)));

    // The unit address is elided by the caller and supplied by the tree. The
    // full name still works.
    assert_eq!(tree.find_node("/memory@1000"), Some(memory));

    // The same 16 bytes read as one address cell and one size cell. If the
    // cell counts were hardcoded to the pair `virt` happens to use, this would
    // still say 0x1000 and 0x2000.
    let narrow = test_tree(1, 1).finish();
    let narrow_memory = narrow.find_node("/memory").expect("no /memory node");
    assert_eq!(narrow.reg(&narrow_memory, 0), Some((0x0, 0x1000)));
    assert_eq!(narrow.reg(&narrow_memory, 1), Some((0x0, 0x2000)));

    // The root is a node like any other, and its own cells come from the
    // default rather than from itself.
    let root = tree.find_node("/").expect("no root node");
    assert_eq!(root.depth, 1);
    assert_eq!(tree.property_str(&root, "compatible"), Some("test,root"));

    // `compatible` is a list. Matching the whole property as one string finds
    // the first entry and misses every other one.
    assert_eq!(tree.find_compatible("test,thing"), Some(memory));
    assert_eq!(tree.find_compatible("test,memory"), Some(memory));
    assert_eq!(tree.find_compatible("test,mem"), None);

    // Absent things are absent, not faults.
    assert_eq!(tree.find_node("/nowhere"), None);
    assert_eq!(tree.find_node("/memory/deeper"), None);
    assert_eq!(tree.property(&memory, "status"), None);
    assert_eq!(tree.reg(&memory, 9), None);

    // A property whose length runs past the end of the blob. Believed, this is
    // a slice of 64 KiB of whatever follows the device tree.
    let mut lying = Tree::new();
    lying.begin("");
    lying.lying_prop(STR_COMPATIBLE, 0x10000);
    lying.end_node();
    let lying = lying.finish();
    let lying_root = lying.find_node("/").expect("no root node");
    assert_eq!(lying.property(&lying_root, "compatible"), None);

    // A node name with no terminator, which asks us to read until we find a
    // zero byte somewhere outside the blob.
    let mut unterminated = Tree::new();
    unterminated.u32(FDT_BEGIN_NODE);
    unterminated.bytes[unterminated.len..unterminated.len + 4].copy_from_slice(b"aaaa");
    unterminated.len += 4;
    let unterminated = unterminated.finish();
    assert_eq!(unterminated.find_node("/"), None);

    // A tree deeper than the walker can hold. The array of cell counts is
    // fixed, so the alternative to stopping is writing past the end of it.
    let mut deep = Tree::new();
    for _ in 0..MAX_DEPTH + 2 {
        deep.begin("");
    }
    let deep = deep.finish();
    let mut walker = deep.walk();
    let mut seen = 0;
    while walker.next().is_some() {
        seen += 1;
    }
    assert_eq!(
        seen,
        MAX_DEPTH - 1,
        "the walker descended past its own array"
    );

    // And the real thing: what QEMU actually handed us this boot.
    if let Some(live) = blob() {
        let root = live.find_node("/").expect("the live tree has no root");
        let model = live.property_str(&root, "compatible").unwrap_or("?");
        let memory = live
            .find_node("/memory")
            .expect("the live tree has no memory");
        let (base, size) = live.reg(&memory, 0).expect("no reg on /memory");

        assert_eq!(base, RAM_BASE, "the tree disagrees about where RAM starts");
        assert_eq!(
            size, RAM_SIZE,
            "the tree disagrees about how much RAM there is"
        );

        println!(
            "passed, this machine is a {model} with {} MiB at {base:#x}",
            size / 1024 / 1024
        );
    } else {
        println!("passed, no live tree to check against");
    }
}
