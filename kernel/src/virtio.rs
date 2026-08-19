//! virtio over MMIO: finding the transports and agreeing on the rules.
//!
//! A virtio device is two halves. The transport is the part that is the same
//! whatever the device does: a window of registers for saying hello, agreeing
//! on features, pointing at queues, and acknowledging interrupts. The device
//! type is the part that differs, and none of it is here.
//!
//! `virt` puts 32 transport slots in the device tree whether or not anything
//! is plugged into them, so most of what this module finds is empty sockets.
//! That is the normal shape of a virtio machine and not a QEMU quirk: the
//! slots are addresses the machine reserved, and a device id of zero is the
//! machine saying nothing is there.
//!
//! # Why version 2 only
//!
//! The legacy interface (version 1) puts a page size in the transport,
//! addresses queues by page number rather than by address, and has a different
//! layout for half the registers. It is all still out there in the world, and
//! supporting it means two code paths through every driver for the sake of
//! hardware this kernel will never meet. QEMU offers version 2 by default.
//! Refusing anything else keeps one path.

use core::ptr::{read_volatile, write_volatile};

use crate::sync::Lock;
use crate::{fdt, paging, print, println};

/// "virt" in little endian, at offset 0 of every transport.
pub const MAGIC: u32 = 0x7472_6976;

/// The only transport version this kernel speaks.
pub const VERSION: u32 = 2;

/// How many slots to look at. `virt` has exactly 32.
pub const MAX_TRANSPORTS: usize = 32;

// Transport registers, from the virtio 1.1 specification.
const MAGIC_VALUE: usize = 0x000;
const VERSION_REG: usize = 0x004;
const DEVICE_ID: usize = 0x008;
const DEVICE_FEATURES: usize = 0x010;
const DEVICE_FEATURES_SEL: usize = 0x014;
const DRIVER_FEATURES: usize = 0x020;
const DRIVER_FEATURES_SEL: usize = 0x024;
const STATUS: usize = 0x070;

// Status bits. The device watches these being set in order, and a driver that
// sets them out of order is telling the device something untrue.
const STATUS_ACKNOWLEDGE: u32 = 1;
const STATUS_DRIVER: u32 = 2;
const STATUS_FEATURES_OK: u32 = 8;
const STATUS_FAILED: u32 = 128;

/// Feature bit 32: the device speaks the non-legacy specification. Required of
/// anything answering on a version 2 transport, and the one bit this kernel
/// insists on before it will talk to a device at all.
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

/// Device ids we can put a name to. There are dozens more.
fn device_name(id: u32) -> &'static str {
    match id {
        0 => "empty",
        1 => "network",
        2 => "block",
        3 => "console",
        4 => "entropy",
        16 => "gpu",
        18 => "input",
        _ => "unknown",
    }
}

/// Why a transport could not be brought up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The window does not contain a virtio transport.
    NotVirtio(u32),
    /// A transport, but one whose registers are laid out differently.
    WrongVersion(u32),
    /// The device does not speak the non-legacy specification.
    Legacy,
    /// The device rejected the features we offered.
    FeaturesRefused,
}

/// One virtio-mmio transport window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Transport {
    /// Physical base of the register window.
    pub base: usize,
    /// The interrupt this device raises, as the controller numbers it.
    pub intid: u32,
    pub device_id: u32,
    /// What the device offered, which is not the same as what was agreed.
    pub features: u64,
}

impl Transport {
    fn read(&self, offset: usize) -> u32 {
        let address = paging::phys_to_virt((self.base + offset) as u64) as *const u32;
        unsafe { read_volatile(address) }
    }

    fn write(&self, offset: usize, value: u32) {
        let address = paging::phys_to_virt((self.base + offset) as u64) as *mut u32;
        unsafe { write_volatile(address, value) }
    }

    /// The 64 feature bits the device is offering, read as two halves.
    fn device_features(&self) -> u64 {
        self.write(DEVICE_FEATURES_SEL, 1);
        let high = self.read(DEVICE_FEATURES) as u64;
        self.write(DEVICE_FEATURES_SEL, 0);
        let low = self.read(DEVICE_FEATURES) as u64;
        (high << 32) | low
    }

    fn set_driver_features(&self, features: u64) {
        self.write(DRIVER_FEATURES_SEL, 1);
        self.write(DRIVER_FEATURES, (features >> 32) as u32);
        self.write(DRIVER_FEATURES_SEL, 0);
        self.write(DRIVER_FEATURES, features as u32);
    }

    /// Put the device back where it started.
    ///
    /// Writing zero to the status register is the reset, and the device is
    /// entitled to take its time about it, so the reset is not complete until
    /// the register reads back as zero.
    pub fn reset(&self) {
        self.write(STATUS, 0);
        while self.read(STATUS) != 0 {
            core::hint::spin_loop();
        }
    }

    /// The handshake every virtio device begins with, whatever it is.
    ///
    /// Reset, say we noticed it, say we have a driver, agree on features, and
    /// have the device confirm the agreement. Everything after this point is
    /// device specific; everything up to it is the same for a disk, a network
    /// card and a random number generator.
    ///
    /// The confirmation is the part worth doing properly. `FEATURES_OK` is
    /// written by the driver and *read back* to see whether the device kept
    /// it: a device that cannot work with what was offered clears the bit, and
    /// a driver that does not check carries on configuring a device that has
    /// already given up.
    pub fn negotiate(&self, wanted: u64) -> Result<u64, Error> {
        self.reset();

        self.write(STATUS, STATUS_ACKNOWLEDGE);
        self.write(STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

        let offered = self.device_features();
        if offered & VIRTIO_F_VERSION_1 == 0 {
            self.write(STATUS, STATUS_FAILED);
            return Err(Error::Legacy);
        }

        let agreed = offered & (wanted | VIRTIO_F_VERSION_1);
        self.set_driver_features(agreed);

        self.write(
            STATUS,
            STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK,
        );

        if self.read(STATUS) & STATUS_FEATURES_OK == 0 {
            self.write(STATUS, STATUS_FAILED);
            return Err(Error::FeaturesRefused);
        }

        Ok(agreed)
    }

    /// The current status register, for anything that wants to check on a
    /// device without owning it.
    pub fn status(&self) -> u32 {
        self.read(STATUS)
    }
}

static TRANSPORTS: Lock<[Option<Transport>; MAX_TRANSPORTS]> = Lock::new([None; MAX_TRANSPORTS]);
static SLOTS_SEEN: Lock<usize> = Lock::new(0);

/// Decode a `virtio,mmio` node's window, magic, version and interrupt.
fn probe(blob: &fdt::Blob, node: &fdt::Node) -> Option<Result<Transport, Error>> {
    let mut windows = [fdt::Region::default(); 1];
    if blob.regions(node, &mut windows) == 0 {
        return None;
    }
    let window = windows[0];

    paging::map_device(window.base, window.size);

    // `interrupts` on a GIC is three cells: which kind of interrupt, its
    // number within that kind, and how it is triggered.
    let interrupts = blob.property(node, "interrupts")?;
    let kind = u32::from_be_bytes(interrupts.get(0..4)?.try_into().ok()?);
    let number = u32::from_be_bytes(interrupts.get(4..8)?.try_into().ok()?);

    // Kind 0 is a shared peripheral interrupt, and the device tree numbers
    // those from zero while the controller numbers them from 32, because the
    // first 32 ids belong to software generated and private interrupts. A
    // driver that skips this offset asks the controller to enable somebody
    // else's interrupt, and then waits forever for its own.
    let intid = match kind {
        0 => number + 32,
        1 => number + 16,
        _ => return None,
    };

    let transport = Transport {
        base: window.base as usize,
        intid,
        device_id: 0,
        features: 0,
    };

    let magic = transport.read(MAGIC_VALUE);
    if magic != MAGIC {
        return Some(Err(Error::NotVirtio(magic)));
    }

    let version = transport.read(VERSION_REG);
    if version != VERSION {
        return Some(Err(Error::WrongVersion(version)));
    }

    Some(Ok(Transport {
        device_id: transport.read(DEVICE_ID),
        features: transport.device_features(),
        ..transport
    }))
}

/// Walk the tree for transports, map them, and record the occupied ones.
///
/// Returns how many slots were looked at and how many had a device in them.
pub fn discover(blob: &fdt::Blob) -> (usize, usize) {
    let mut table = [None; MAX_TRANSPORTS];
    let mut slots = 0;
    let mut occupied = 0;
    let mut offset = 0;

    while let Some(node) = blob.find_compatible_after("virtio,mmio", offset) {
        offset = node.offset;
        slots += 1;

        match probe(blob, &node) {
            // An empty slot is not a failure. It is the machine saying it
            // reserved an address for a device nobody plugged in.
            Some(Ok(transport)) if transport.device_id == 0 => {}
            Some(Ok(transport)) => {
                if occupied < MAX_TRANSPORTS {
                    table[occupied] = Some(transport);
                    occupied += 1;
                }
            }
            Some(Err(error)) => println!("virtio: {:#x} refused ({error:?})", node.offset),
            None => {}
        }

        if slots == MAX_TRANSPORTS {
            break;
        }
    }

    *TRANSPORTS.lock() = table;
    *SLOTS_SEEN.lock() = slots;

    (slots, occupied)
}

/// The transports that had a device in them.
pub fn transports() -> [Option<Transport>; MAX_TRANSPORTS] {
    *TRANSPORTS.lock()
}

/// The first device of a given type, if the machine has one.
pub fn find(device_id: u32) -> Option<Transport> {
    transports()
        .into_iter()
        .flatten()
        .find(|transport| transport.device_id == device_id)
}

/// What was found, for the boot log.
pub fn print_table(slots: usize, occupied: usize) {
    println!("virtio: {slots} mmio slots, {occupied} occupied");

    for transport in transports().into_iter().flatten() {
        println!(
            "  {:#012x} : {} device, intid {}, offers {:#x}",
            transport.base,
            device_name(transport.device_id),
            transport.intid,
            transport.features
        );
    }
}

/// What every occupied transport must look like, and the handshake each one
/// has to complete.
pub fn self_test() {
    print!("virtio self test: ");

    let found: usize = transports().iter().flatten().count();
    assert!(
        found > 0,
        "no virtio devices; the runner is supposed to attach one"
    );

    let mut previous_intid = 0;
    let mut negotiated = 0;

    for transport in transports().into_iter().flatten() {
        // Read back rather than trusted from discovery: this is the first
        // access through the mapping that `map_device` made, so a window that
        // was found but not mapped fails here rather than in a driver.
        assert_eq!(
            transport.read(MAGIC_VALUE),
            MAGIC,
            "transport at {:#x} lost its magic",
            transport.base
        );
        assert_eq!(transport.read(VERSION_REG), VERSION);
        assert_ne!(transport.device_id, 0, "an empty slot was recorded");

        // Shared peripheral interrupts start at 32, and every device has its
        // own line.
        assert!(
            transport.intid >= 32,
            "intid {} is not an SPI",
            transport.intid
        );
        assert!(transport.intid < crate::gic::num_interrupts());
        assert_ne!(transport.intid, previous_intid);
        previous_intid = transport.intid;

        // The whole handshake, ending with the device confirming the features
        // rather than the driver assuming them.
        let agreed = transport
            .negotiate(0)
            .expect("device would not agree to the bare specification");
        assert_eq!(
            agreed & VIRTIO_F_VERSION_1,
            VIRTIO_F_VERSION_1,
            "a version 2 transport must speak the non-legacy specification"
        );
        assert_eq!(
            transport.status() & STATUS_FEATURES_OK,
            STATUS_FEATURES_OK,
            "the device dropped FEATURES_OK"
        );

        // Leave it where it was found. The driver in #47 does its own
        // handshake, and a device left half configured by a boot time probe is
        // a device that behaves differently depending on whether it was
        // probed.
        transport.reset();
        assert_eq!(transport.status(), 0, "reset did not take");

        negotiated += 1;
    }

    println!(
        "passed, {negotiated} {} agreed on features and {} reset",
        if negotiated == 1 { "device" } else { "devices" },
        if negotiated == 1 { "was" } else { "were" }
    );
}
