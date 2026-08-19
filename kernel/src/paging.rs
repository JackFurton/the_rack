//! The MMU: virtual memory, enforced permissions, and the move to the high
//! half.
//!
//! Until now every address has been physical and nothing has been protected.
//! The kernel could write to its own code and dereference null happily.
//!
//! # Layout
//!
//! 4 KiB granule, 48 bit virtual addresses, four levels of table. Each level
//! consumes 9 bits of address and has 512 entries:
//!
//!   level 0   bits [47:39]   each entry covers 512 GiB
//!   level 1   bits [38:30]   each entry covers   1 GiB
//!   level 2   bits [29:21]   each entry covers   2 MiB
//!   level 3   bits [20:12]   each entry covers   4 KiB
//!
//! aarch64 has two independent table roots, selected by the top bits of the
//! address. `TTBR0_EL1` translates the low half and `TTBR1_EL1` the high half.
//! That split is the whole reason kernels live high: the kernel keeps
//! `TTBR1_EL1` permanently, and each task gets its own `TTBR0_EL1`, so
//! switching address spaces never touches the kernel's mappings.
//!
//! # The order of operations matters
//!
//! Enabling the MMU changes the meaning of the program counter mid-stream. The
//! instruction after the one that sets `SCTLR_EL1.M` is fetched through the
//! translation tables. If the code we are executing is not mapped at the
//! address we are executing it from, the machine dies immediately with no
//! output and no fault report.
//!
//! So `TTBR0_EL1` gets an identity map first, we enable with that safety net
//! in place, and only then jump to the high alias and drop the identity map.

use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::frames::{self, FRAME_SIZE, Frame, RAM_BASE, RAM_SIZE};
use crate::println;

/// Base of the kernel's half of the address space.
///
/// Every physical address has a fixed alias at `KERNEL_BASE + pa`, which keeps
/// the translation between the two a single addition rather than a lookup.
pub const KERNEL_BASE: u64 = 0xffff_0000_0000_0000;

const PAGE_SIZE: u64 = FRAME_SIZE;
const BLOCK_2MIB: u64 = 2 * 1024 * 1024;

/// Output address field of a descriptor, bits [47:12].
const ADDR_MASK: u64 = 0x0000_ffff_ffff_f000;

// Descriptor type bits.
const VALID: u64 = 1 << 0;
/// At levels 0 to 2 this means "table". At level 3 it means "page". A level 3
/// descriptor with this bit clear is not a block, it is invalid, which is an
/// easy and silent mistake to make.
const TABLE_OR_PAGE: u64 = 1 << 1;

// Lower attributes.
const ATTR_INDEX_SHIFT: u64 = 2;
const AP_SHIFT: u64 = 6;
const SH_SHIFT: u64 = 8;
/// Access flag. Hardware raises an access flag fault if this is clear and
/// nothing in software manages it. Forgetting it is the classic way to get a
/// perfectly correct looking table that faults on every access.
const AF: u64 = 1 << 10;

// Upper attributes.
/// Privileged execute never: EL1 may not fetch instructions from here.
const PXN: u64 = 1 << 53;
/// Unprivileged execute never: EL0 may not fetch instructions from here.
const UXN: u64 = 1 << 54;

// Access permissions, bits [7:6].
const AP_RW_EL1: u64 = 0b00;
const AP_RW_ALL: u64 = 0b01;
const AP_RO_EL1: u64 = 0b10;
const AP_RO_ALL: u64 = 0b11;

// Shareability, bits [9:8].
const SH_NON_SHAREABLE: u64 = 0b00;
const SH_INNER_SHAREABLE: u64 = 0b11;

// MAIR_EL1 attribute slots, referenced by index from each descriptor.
const MAIR_DEVICE_INDEX: u64 = 0;
const MAIR_NORMAL_INDEX: u64 = 1;
/// Slot 0 is Device-nGnRnE and slot 1 is Normal write-back read/write-allocate.
///
/// Device-nGnRnE encodes as 0x00, which is why only slot 1 appears here.
const MAIR_ATTR_NORMAL: u64 = 0xff;
const MAIR_VALUE: u64 = MAIR_ATTR_NORMAL << 8;

/// Offset added to a physical address to reach its virtual alias.
///
/// Zero while the MMU is off and physical addresses are directly reachable,
/// `KERNEL_BASE` once we are running in the high half.
///
/// An atomic is fine here even before the MMU is on, unlike the compare
/// exchange loop `sync.rs` avoids. Plain loads and stores compile to `LDR` and
/// `STR`; it is only read-modify-write that needs `LDXR`/`STXR` and therefore
/// the exclusive monitor.
static PHYS_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Virtual address currently aliasing physical address `pa`.
pub fn phys_to_virt(pa: u64) -> u64 {
    pa + PHYS_OFFSET.load(Ordering::Relaxed)
}

/// Memory type and permissions for a mapping.
#[derive(Clone, Copy)]
pub struct Attributes(u64);

impl Attributes {
    /// Kernel code: readable and executable at EL1, never writable, never
    /// executable at EL0.
    pub const fn kernel_text() -> Self {
        Self(
            (MAIR_NORMAL_INDEX << ATTR_INDEX_SHIFT)
                | (AP_RO_EL1 << AP_SHIFT)
                | (SH_INNER_SHAREABLE << SH_SHIFT)
                | AF
                | UXN,
        )
    }

    /// Kernel constants: readable only, never executable.
    pub const fn kernel_rodata() -> Self {
        Self(
            (MAIR_NORMAL_INDEX << ATTR_INDEX_SHIFT)
                | (AP_RO_EL1 << AP_SHIFT)
                | (SH_INNER_SHAREABLE << SH_SHIFT)
                | AF
                | UXN
                | PXN,
        )
    }

    /// Kernel data and free memory: readable and writable, never executable.
    pub const fn kernel_data() -> Self {
        Self(
            (MAIR_NORMAL_INDEX << ATTR_INDEX_SHIFT)
                | (AP_RW_EL1 << AP_SHIFT)
                | (SH_INNER_SHAREABLE << SH_SHIFT)
                | AF
                | UXN
                | PXN,
        )
    }

    /// Memory mapped device registers.
    ///
    /// Device memory rather than Normal, so the hardware performs exactly the
    /// accesses we wrote, in the order we wrote them. Normal memory permits
    /// reordering, merging and speculative reads, all of which are catastrophic
    /// against a UART data register or a GIC acknowledge register.
    pub const fn device() -> Self {
        Self(
            (MAIR_DEVICE_INDEX << ATTR_INDEX_SHIFT)
                | (AP_RW_EL1 << AP_SHIFT)
                | (SH_NON_SHAREABLE << SH_SHIFT)
                | AF
                | UXN
                | PXN,
        )
    }
}

/// Index into the table at `level` for virtual address `va`.
///
/// The mask discards the sign extension bits above 47, so high half addresses
/// index correctly without any special casing.
fn table_index(va: u64, level: usize) -> usize {
    let shift = 39 - 9 * level;
    ((va >> shift) & 0x1ff) as usize
}

/// Pointer to a table's entries, through whatever alias currently works.
fn table_ptr(table: Frame) -> *mut u64 {
    phys_to_virt(table.addr()) as *mut u64
}

fn read_entry(table: Frame, index: usize) -> u64 {
    unsafe { read_volatile(table_ptr(table).add(index)) }
}

fn write_entry(table: Frame, index: usize, value: u64) {
    unsafe { write_volatile(table_ptr(table).add(index), value) }
}

/// Follow the entry at `index`, allocating a table if there is not one yet.
fn next_table(table: Frame, index: usize) -> Frame {
    let entry = read_entry(table, index);

    if entry & VALID != 0 {
        assert!(
            entry & TABLE_OR_PAGE != 0,
            "wanted a table but found a block descriptor; \
             a larger mapping already covers this address"
        );
        return Frame::from_addr(entry & ADDR_MASK);
    }

    let new = frames::alloc().expect("out of frames while building page tables");
    // Frames arrive zeroed, so every entry in the new table is already
    // invalid. That is exactly what we want and why the allocator's zeroing
    // guarantee matters here.
    write_entry(table, index, new.addr() | VALID | TABLE_OR_PAGE);
    new
}

/// Map one 4 KiB page.
fn map_page(root: Frame, va: u64, pa: u64, attrs: Attributes) {
    let mut table = root;
    for level in 0..3 {
        table = next_table(table, table_index(va, level));
    }

    let index = table_index(va, 3);
    // A level 3 descriptor needs both bits set. With only VALID it is not a
    // block, it is reserved, and the walk faults.
    write_entry(table, index, pa | attrs.0 | VALID | TABLE_OR_PAGE);
}

/// Map one 2 MiB block, which stops the walk a level early.
fn map_block(root: Frame, va: u64, pa: u64, attrs: Attributes) {
    let mut table = root;
    for level in 0..2 {
        table = next_table(table, table_index(va, level));
    }

    let index = table_index(va, 2);
    // Block descriptor: VALID set, TABLE_OR_PAGE clear.
    write_entry(table, index, pa | attrs.0 | VALID);
}

/// Map `size` bytes, using 2 MiB blocks wherever alignment allows.
///
/// Blocks matter more than they look. Mapping 256 MiB of RAM with 4 KiB pages
/// needs 65536 descriptors across 128 tables; with blocks it needs 128
/// descriptors and no level 3 tables at all.
pub fn map_range(root: Frame, va: u64, pa: u64, size: u64, attrs: Attributes) {
    let mut offset = 0;

    while offset < size {
        let remaining = size - offset;
        let va = va + offset;
        let pa = pa + offset;

        if remaining >= BLOCK_2MIB && va.is_multiple_of(BLOCK_2MIB) && pa.is_multiple_of(BLOCK_2MIB)
        {
            map_block(root, va, pa, attrs);
            offset += BLOCK_2MIB;
        } else {
            map_page(root, va, pa, attrs);
            offset += PAGE_SIZE;
        }
    }
}

unsafe extern "C" {
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __data_start: u8;
    static __kernel_end: u8;
}

fn symbol(sym: &u8) -> u64 {
    (sym as *const u8) as u64
}

/// Device MMIO we currently touch: GIC distributor at 0x0800_0000, GIC CPU
/// interface at 0x0801_0000, PL011 UART at 0x0900_0000.
const DEVICE_BASE: u64 = 0x0800_0000;
const DEVICE_SIZE: u64 = 32 * 1024 * 1024;

/// Fill in one root with the whole kernel address space.
///
/// `offset` is added to every virtual address, so the same routine builds both
/// the identity map (offset 0) and the high half map (offset `KERNEL_BASE`).
/// Building both from one description is the point: a permission that differs
/// between them would be a bug that only appears after the jump.
fn build(root: Frame, offset: u64) {
    let text_start = symbol(unsafe { &__text_start });
    let text_end = symbol(unsafe { &__text_end });
    let rodata_start = symbol(unsafe { &__rodata_start });
    let rodata_end = symbol(unsafe { &__rodata_end });
    let data_start = symbol(unsafe { &__data_start });
    let kernel_end = symbol(unsafe { &__kernel_end });

    // Everything below the kernel image: the device tree blob, and the gap
    // QEMU leaves before our load address.
    map_range(
        root,
        RAM_BASE + offset,
        RAM_BASE,
        text_start - RAM_BASE,
        Attributes::kernel_data(),
    );

    map_range(
        root,
        text_start + offset,
        text_start,
        text_end - text_start,
        Attributes::kernel_text(),
    );

    map_range(
        root,
        rodata_start + offset,
        rodata_start,
        rodata_end - rodata_start,
        Attributes::kernel_rodata(),
    );

    // Data, BSS, and the boot stack.
    map_range(
        root,
        data_start + offset,
        data_start,
        kernel_end - data_start,
        Attributes::kernel_data(),
    );

    // The rest of RAM, which the frame allocator hands out.
    map_range(
        root,
        kernel_end + offset,
        kernel_end,
        RAM_BASE + RAM_SIZE - kernel_end,
        Attributes::kernel_data(),
    );

    map_range(
        root,
        DEVICE_BASE + offset,
        DEVICE_BASE,
        DEVICE_SIZE,
        Attributes::device(),
    );
}

/// Build both roots and return them as `(ttbr0, ttbr1)`.
pub fn build_tables() -> (Frame, Frame) {
    let low = frames::alloc().expect("no frame for the low root");
    let high = frames::alloc().expect("no frame for the high root");

    // Identity, so the program counter stays valid across the moment the MMU
    // turns on.
    build(low, 0);
    // The permanent kernel mapping.
    build(high, KERNEL_BASE);

    (low, high)
}

/// Largest physical address this CPU supports, encoded for `TCR_EL1.IPS`.
///
/// Read from the CPU rather than assumed. Claiming more physical address bits
/// than the hardware implements is a configuration the architecture calls
/// unpredictable.
fn parange() -> u64 {
    let mmfr0: u64;
    unsafe { asm!("mrs {}, id_aa64mmfr0_el1", out(reg) mmfr0, options(nomem, nostack)) };
    (mmfr0 & 0b1111).min(0b101)
}

/// Turn the MMU on, with both roots live.
///
/// # Safety
///
/// `ttbr0` must contain a valid identity mapping of the code calling this, or
/// the next instruction fetch faults in an unrecoverable way.
pub unsafe fn enable(ttbr0: Frame, ttbr1: Frame) {
    // 48 bit address spaces: 64 - 48.
    let t0sz: u64 = 16;
    let t1sz: u64 = 16;
    // Write-back read/write-allocate cacheable table walks, inner shareable.
    let rgn: u64 = 0b01;
    let sh: u64 = 0b11;

    let tcr = t0sz
        | (rgn << 8)
        | (rgn << 10)
        | (sh << 12)
        // TG0 is bits [15:14] and a 4 KiB granule encodes as 0b00, so the
        // field is correct by being absent here.
        | (t1sz << 16)
        | (rgn << 24)
        | (rgn << 26)
        | (sh << 28)
        // TG1 uses a different encoding from TG0 for the same granule size.
        // 0b10 is 4 KiB here. Copying the TG0 value into TG1 is a popular way
        // to configure a 16 KiB granule by accident.
        | (0b10 << 30)
        | (parange() << 32);

    unsafe {
        asm!(
            "msr mair_el1, {mair}",
            "msr tcr_el1, {tcr}",
            "msr ttbr0_el1, {ttbr0}",
            "msr ttbr1_el1, {ttbr1}",

            // Every table write above must be visible to the table walker, and
            // no stale translation may survive, before the MMU consults any of
            // it.
            "dsb ish",
            "tlbi vmalle1",
            "dsb ish",
            "isb",

            // SCTLR_EL1: M enables translation, C the data cache, I the
            // instruction cache. The caches are not an optimisation here: with
            // the MMU off everything was Device memory, and turning
            // translation on without them would leave the kernel uncached.
            "mrs {tmp}, sctlr_el1",
            "orr {tmp}, {tmp}, #(1 << 0)",
            "orr {tmp}, {tmp}, #(1 << 2)",
            "orr {tmp}, {tmp}, #(1 << 12)",
            "msr sctlr_el1, {tmp}",

            // From the instruction after this barrier, the program counter is
            // a virtual address.
            "isb",

            mair = in(reg) MAIR_VALUE,
            tcr = in(reg) tcr,
            ttbr0 = in(reg) ttbr0.addr(),
            ttbr1 = in(reg) ttbr1.addr(),
            tmp = out(reg) _,
            options(nostack),
        );
    }
}

/// Move execution to the high half and continue at `continuation`, passing it
/// `arg`.
///
/// The argument exists because the device tree pointer has to survive the jump
/// and there is nowhere else to put it: a static written before this point was
/// written through a low address, and the whole point of the jump is that low
/// addresses stop meaning anything.
///
/// # Safety
///
/// The MMU must be on with a high half mapping of the kernel already in
/// `TTBR1_EL1`, and the current stack must have a high alias.
pub unsafe fn jump_to_high_half(continuation: extern "C" fn(u64) -> !, arg: u64) -> ! {
    let target = continuation as usize as u64 + KERNEL_BASE;

    unsafe {
        asm!(
            // The stack pointer is still a low address, and the frames it
            // points at are about to become unreachable. Same physical memory,
            // different name.
            "add sp, sp, {offset}",
            // An indirect branch, because a normal `b` is PC relative and
            // would land us right back where we started.
            "br {target}",
            offset = in(reg) KERNEL_BASE,
            target = in(reg) target,
            // Placed by name, since the continuation reads it as an argument.
            in("x0") arg,
            options(noreturn),
        );
    }
}

/// Finish the move: point everything at high addresses and retire the identity
/// map.
///
/// # Safety
///
/// Must be called only from code already executing in the high half.
pub unsafe fn finish_high_half() {
    // From here every physical address must be translated before use. This has
    // to happen before anything touches a device register or a frame.
    PHYS_OFFSET.store(KERNEL_BASE, Ordering::Relaxed);

    unsafe {
        let vbar: u64;
        asm!("mrs {}, vbar_el1", out(reg) vbar, options(nomem, nostack));
        asm!("msr vbar_el1, {}", "isb", in(reg) vbar + KERNEL_BASE, options(nostack));

        // Retire the identity map. TCR_EL1.EPD0 stops the walker consulting
        // TTBR0 at all, so low addresses now take a translation fault instead
        // of quietly working. Null dereferences become loud.
        let mut tcr: u64;
        asm!("mrs {}, tcr_el1", out(reg) tcr, options(nomem, nostack));
        tcr |= 1 << 7;
        asm!(
            "msr tcr_el1, {tcr}",
            "msr ttbr0_el1, xzr",
            "dsb ish",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            tcr = in(reg) tcr,
            options(nostack),
        );
    }
}

/// Report the translation configuration, read back from the hardware rather
/// than from what we believe we wrote.
pub fn print_config() {
    let (sctlr, tcr, ttbr0, ttbr1): (u64, u64, u64, u64);
    unsafe {
        asm!("mrs {}, sctlr_el1", out(reg) sctlr, options(nomem, nostack));
        asm!("mrs {}, tcr_el1", out(reg) tcr, options(nomem, nostack));
        asm!("mrs {}, ttbr0_el1", out(reg) ttbr0, options(nomem, nostack));
        asm!("mrs {}, ttbr1_el1", out(reg) ttbr1, options(nomem, nostack));
    }

    println!("mmu: enabled, 4 KiB granule, 48 bit addresses");
    println!(
        "  sctlr  : {sctlr:#018x}  mmu={} dcache={} icache={}",
        sctlr & 1,
        (sctlr >> 2) & 1,
        (sctlr >> 12) & 1
    );
    println!("  tcr    : {tcr:#018x}  ttbr0 walks {}", {
        if tcr & (1 << 7) != 0 {
            "disabled"
        } else {
            "enabled"
        }
    });
    println!("  ttbr0  : {ttbr0:#018x}");
    println!("  ttbr1  : {ttbr1:#018x}");
    println!("  kernel : {KERNEL_BASE:#018x} + physical");
}

/// Prove the mappings do what they claim.
///
/// Reading these assertions is the point. "The MMU is on" is easy and almost
/// meaningless; a table full of read-write-execute mappings would satisfy it.
/// What matters is that the restrictions are real, and the only way to know
/// that is to try the things that should be refused and confirm they are.
pub fn self_test() {
    // Symbols and statics need no fixing up after the jump. `adrp` is PC
    // relative, and the offset it encodes is a link-time constant, so with the
    // program counter in the high half every symbol resolves to its high alias
    // automatically. That is the whole reason a single linear offset works
    // without relinking the kernel.
    let running_at = self_test as *const () as u64;
    assert!(
        running_at >= KERNEL_BASE,
        "self test is not running in the high half"
    );

    let text = symbol(unsafe { &__text_start });
    let rodata = symbol(unsafe { &__rodata_start });

    // Executable memory must still be readable, or we have broken more than
    // we fixed.
    let instruction = unsafe { read_volatile(text as *const u32) };
    assert_ne!(instruction, 0, "kernel text reads as zero");

    let write_text = crate::exceptions::probe(|| unsafe {
        write_volatile(text as *mut u32, 0xdead_beef);
    });
    assert!(write_text.faulted(), "kernel text is writable");

    let write_rodata = crate::exceptions::probe(|| unsafe {
        write_volatile(rodata as *mut u8, 0xff);
    });
    assert!(write_rodata.faulted(), "kernel rodata is writable");

    // The identity map is retired, so a low address is now nothing at all.
    // This is what makes a null dereference a fault rather than a quiet read
    // of whatever happens to live at physical zero.
    let low_half = crate::exceptions::probe(|| unsafe {
        let _ = read_volatile(0x1000 as *const u64);
    });
    assert!(low_half.faulted(), "the identity map is still live");

    // The instruction is intact: the refused write did not partially land.
    assert_eq!(
        unsafe { read_volatile(text as *const u32) },
        instruction,
        "a faulting write still modified memory"
    );

    println!("paging self test: passed, running at {running_at:#018x}");
    println!("  write to .text   : {}", write_text.describe());
    println!("  write to .rodata : {}", write_rodata.describe());
    println!("  read low half    : {}", low_half.describe());
}

impl Attributes {
    /// User data: readable and writable at both EL0 and EL1, never executable.
    pub const fn user_data() -> Self {
        Self(
            (MAIR_NORMAL_INDEX << ATTR_INDEX_SHIFT)
                | (AP_RW_ALL << AP_SHIFT)
                | (SH_INNER_SHAREABLE << SH_SHIFT)
                | AF
                | UXN
                | PXN,
        )
    }

    /// User code: executable at EL0, read-only, and explicitly *not*
    /// executable by the kernel.
    ///
    /// PXN matters more than it looks. Without it, any kernel bug that
    /// redirects control flow into a user-controlled page executes attacker
    /// chosen instructions with full privilege.
    pub const fn user_text() -> Self {
        Self(
            (MAIR_NORMAL_INDEX << ATTR_INDEX_SHIFT)
                | (AP_RO_ALL << AP_SHIFT)
                | (SH_INNER_SHAREABLE << SH_SHIFT)
                | AF
                | PXN,
        )
    }
}

/// A low half address space, the thing `TTBR0_EL1` points at.
///
/// One per task. Switching tasks swaps this and leaves `TTBR1_EL1` alone,
/// which is the entire reason the kernel was moved to the high half in #5:
/// changing address spaces cannot disturb the kernel's own mappings.
pub struct AddressSpace {
    root: Frame,
}

impl Default for AddressSpace {
    fn default() -> Self {
        Self::new()
    }
}

impl AddressSpace {
    /// An address space with nothing in it. Every low address faults.
    pub fn new() -> Self {
        Self {
            root: frames::alloc().expect("no frame for an address space root"),
        }
    }

    pub fn map(&self, va: u64, pa: u64, size: u64, attrs: Attributes) {
        assert!(
            va < KERNEL_BASE,
            "an address space only covers the low half; {va:#x} belongs to the kernel"
        );
        map_range(self.root, va, pa, size, attrs);
    }

    pub fn root(&self) -> Frame {
        self.root
    }
}

/// Make `root` the current low half address space.
///
/// # Safety
///
/// `root` must be a valid level 0 table, and the caller must not be executing
/// from or relying on any low address across the change.
pub unsafe fn activate_root(root: Frame) {
    unsafe {
        asm!(
            "msr ttbr0_el1, {root}",
            "dsb ish",
            // Invalidates every stage 1 entry for EL1&0, kernel mappings
            // included, which is heavier than it needs to be. ASIDs are the
            // right answer: tag each address space and invalidate only its
            // entries. Worth doing when there is enough switching for it to
            // matter, and noted here so the current choice is a decision
            // rather than an oversight.
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            root = in(reg) root.addr(),
            options(nostack),
        );
    }
}

/// Let the walker consult `TTBR0_EL1` again.
///
/// Enabling the MMU set `TCR_EL1.EPD0` to retire the identity map, which turned
/// every low address into a translation fault. Now that address spaces exist,
/// low addresses have to be translatable again, so the protection comes from
/// the tables rather than from refusing to walk them at all. A task with no
/// address space gets an empty root, which faults on everything just the same.
///
/// # Safety
///
/// `TTBR0_EL1` must already point at a valid level 0 table. Clearing `EPD0`
/// tells the walker to start trusting that register, so calling this while it
/// holds a stale or arbitrary value hands the walker whatever happens to be
/// there to interpret as page tables.
pub unsafe fn enable_user_translation() {
    unsafe {
        let mut tcr: u64;
        asm!("mrs {}, tcr_el1", out(reg) tcr, options(nomem, nostack));
        tcr &= !(1 << 7);
        asm!(
            "msr tcr_el1, {tcr}",
            "dsb ish",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            tcr = in(reg) tcr,
            options(nostack),
        );
    }
}

/// Walk `root` for `va` and return the leaf descriptor, if the address maps to
/// anything at all.
///
/// Stops early on a block descriptor, which is a leaf at level 1 or 2 rather
/// than a pointer to another table.
pub fn lookup(root: Frame, va: u64) -> Option<u64> {
    let mut table = root;

    for level in 0..3 {
        let entry = read_entry(table, table_index(va, level));
        if entry & VALID == 0 {
            return None;
        }
        if entry & TABLE_OR_PAGE == 0 {
            return Some(entry);
        }
        table = Frame::from_addr(entry & ADDR_MASK);
    }

    let entry = read_entry(table, table_index(va, 3));
    (entry & VALID != 0).then_some(entry)
}

/// Could EL0 read every byte of `va..va + len` through `root`?
///
/// This is the question a syscall has to answer about a pointer it was handed,
/// and it is not the same as "can the kernel read it". The kernel can read
/// plenty this task cannot. Reading the access permission bits out of the
/// task's own tables asks the hardware's question rather than a convenient
/// approximation of it.
pub fn user_readable(root: Frame, va: u64, len: u64) -> bool {
    // AP is two bits: the low one says writable, the high one says EL0 may
    // touch it at all. 0b01 and 0b11 are the EL0-accessible encodings.
    user_range(root, va, len, |ap| ap == AP_RW_ALL || ap == AP_RO_ALL)
}

/// Could EL0 write every byte of `va..va + len` through `root`?
///
/// The read check's sibling, and needed for the same reason: a syscall that
/// fills in a buffer on a task's behalf is about to write through a pointer
/// the task chose. `AP_RW_ALL` is the only encoding that lets EL0 write, so
/// the read-only user encoding is refused here even though it passes the read
/// check.
pub fn user_writable(root: Frame, va: u64, len: u64) -> bool {
    user_range(root, va, len, |ap| ap == AP_RW_ALL)
}

/// Shared walk behind the two permission checks.
fn user_range(root: Frame, va: u64, len: u64, allowed: impl Fn(u64) -> bool) -> bool {
    if len == 0 {
        return true;
    }

    // Reject the low half boundary case before doing arithmetic that could
    // wrap. A length chosen to overflow the addition is exactly the sort of
    // argument this function exists to refuse.
    let Some(end) = va.checked_add(len) else {
        return false;
    };
    if end > KERNEL_BASE {
        return false;
    }

    let mut page = va & !(PAGE_SIZE - 1);
    while page < end {
        let Some(entry) = lookup(root, page) else {
            return false;
        };
        if !allowed((entry >> AP_SHIFT) & 0b11) {
            return false;
        }
        page += PAGE_SIZE;
    }

    true
}

/// Virtual to physical through `root`, or `None` if nothing is mapped.
///
/// Tracks the level it stopped at rather than reusing `lookup`, because a
/// block descriptor's address field is aligned to the block rather than to a
/// page: taking the low bits from the wrong granule lands somewhere plausible
/// and wrong, which is the worst kind of wrong.
pub fn translate(root: Frame, va: u64) -> Option<u64> {
    let mut table = root;

    for level in 0..3 {
        let entry = read_entry(table, table_index(va, level));
        if entry & VALID == 0 {
            return None;
        }
        if entry & TABLE_OR_PAGE == 0 {
            // Only levels 1 and 2 may hold blocks with a 4 KiB granule, at
            // 1 GiB and 2 MiB respectively.
            let size = match level {
                1 => 512 * BLOCK_2MIB,
                2 => BLOCK_2MIB,
                _ => return None,
            };
            return Some((entry & ADDR_MASK & !(size - 1)) | (va & (size - 1)));
        }
        table = Frame::from_addr(entry & ADDR_MASK);
    }

    let entry = read_entry(table, table_index(va, 3));
    (entry & VALID != 0).then_some((entry & ADDR_MASK) | (va & (PAGE_SIZE - 1)))
}

/// Copy `len` bytes from one address space into another.
///
/// The kernel is the only thing that can do this, and it is why message
/// passing has to go through a syscall at all: neither task can see the
/// other's memory, and only one low half is in `TTBR0_EL1` at a time.
///
/// It copies through the high half physical map rather than swapping
/// `TTBR0_EL1` twice per chunk. Swapping would mean a TLB invalidate on each
/// side of every chunk, and a window where the running task's own address
/// space is not loaded, which is a hazard to reason about for no gain. The
/// high half already maps all of physical memory, so both sides are reachable
/// at once.
///
/// Walks both spaces per chunk, so the two sides do not need matching page
/// offsets or matching alignment. Returns false if any page on either side is
/// unmapped, which the caller should have ruled out already; checking anyway
/// costs a comparison and turns a kernel-side wild write into a refusal.
pub fn copy_across(src_root: Frame, src_va: u64, dst_root: Frame, dst_va: u64, len: u64) -> bool {
    let mut done = 0;

    while done < len {
        let src = src_va + done;
        let dst = dst_va + done;

        let (Some(src_pa), Some(dst_pa)) = (translate(src_root, src), translate(dst_root, dst))
        else {
            return false;
        };

        // Stop at whichever page boundary comes first. The two buffers can sit
        // at different offsets within their pages, so neither side's boundary
        // can be assumed to be the limit.
        let src_left = PAGE_SIZE - (src & (PAGE_SIZE - 1));
        let dst_left = PAGE_SIZE - (dst & (PAGE_SIZE - 1));
        let chunk = (len - done).min(src_left).min(dst_left);

        unsafe {
            core::ptr::copy_nonoverlapping(
                phys_to_virt(src_pa) as *const u8,
                phys_to_virt(dst_pa) as *mut u8,
                chunk as usize,
            );
        }

        done += chunk;
    }

    true
}

/// Entries in a translation table at any level.
const ENTRIES: usize = 512;

impl AddressSpace {
    /// Free every frame this address space owns: the tables themselves and the
    /// memory they map.
    ///
    /// Takes `self` by value because an address space cannot be used again
    /// afterwards, and the type system may as well say so.
    ///
    /// Assumes this space owns everything mapped in it. That holds while each
    /// task's memory is private; the moment anything is shared between address
    /// spaces this needs reference counting instead, and freeing a shared
    /// frame here would hand a live page back to the allocator.
    pub fn destroy(self) {
        free_table(self.root, 0);
    }
}

fn free_table(table: Frame, level: usize) {
    for index in 0..ENTRIES {
        let entry = read_entry(table, index);
        if entry & VALID == 0 {
            continue;
        }

        let target = Frame::from_addr(entry & ADDR_MASK);

        if level < 3 && entry & TABLE_OR_PAGE != 0 {
            free_table(target, level + 1);
            continue;
        }

        // A leaf. At level 3 that is one frame; at levels 1 and 2 it is a
        // block covering many, and freeing only its first frame would leak the
        // rest while leaving the allocator convinced they were still in use.
        let frames_covered = match level {
            1 => ENTRIES * ENTRIES,
            2 => ENTRIES,
            3 => 1,
            _ => panic!("a level {level} descriptor cannot be a leaf"),
        };
        frames::free_contiguous(target, frames_covered);
    }

    frames::free(table);
}
