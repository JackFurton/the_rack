//! Driver for the ARM PL011 UART.
//!
//! This is the only way the kernel can talk to the outside world right now.
//! Everything else we build gets debugged through this one device, so it is
//! worth getting right rather than poking the data register and hoping.
//!
//! On the QEMU `virt` machine the first PL011 is mapped at 0x0900_0000.

use core::fmt;
use core::ptr::{read_volatile, write_volatile};

use crate::sync::Lock;

const PL011_BASE: usize = 0x0900_0000;

// Register offsets from the PL011 technical reference manual.
const DR: usize = 0x00; // Data
const FR: usize = 0x18; // Flag
const IBRD: usize = 0x24; // Integer baud rate divisor
const FBRD: usize = 0x28; // Fractional baud rate divisor
const LCR_H: usize = 0x2c; // Line control
const CR: usize = 0x30; // Control
const IMSC: usize = 0x38; // Interrupt mask set/clear
const ICR: usize = 0x44; // Interrupt clear

const FR_BUSY: u32 = 1 << 3;
const FR_TXFF: u32 = 1 << 5; // Transmit FIFO full

const LCR_H_FEN: u32 = 1 << 4; // Enable FIFOs
const LCR_H_WLEN_8: u32 = 0b11 << 5; // 8 data bits

const CR_UARTEN: u32 = 1 << 0;
const CR_TXE: u32 = 1 << 8;
const CR_RXE: u32 = 1 << 9;

pub struct Uart {
    base: usize,
}

impl Uart {
    /// # Safety
    /// `base` must be the MMIO base of a PL011, and nothing else may be
    /// driving that device at the same time.
    pub const unsafe fn new(base: usize) -> Self {
        Self { base }
    }

    unsafe fn read(&self, offset: usize) -> u32 {
        unsafe { read_volatile((self.base + offset) as *const u32) }
    }

    unsafe fn write(&self, offset: usize, value: u32) {
        unsafe { write_volatile((self.base + offset) as *mut u32, value) }
    }

    /// Configure for 115200 baud, 8N1, FIFOs on.
    ///
    /// QEMU does not actually care about the baud rate, but real silicon does
    /// and we would rather find out now than during a hardware bring-up.
    pub fn init(&self) {
        unsafe {
            // Disable the UART before touching its configuration.
            self.write(CR, 0);

            // Wait for any character still in flight to finish.
            while self.read(FR) & FR_BUSY != 0 {
                core::hint::spin_loop();
            }

            // 24 MHz reference clock, 115200 baud:
            //   divisor = 24_000_000 / (16 * 115_200) = 13.0208
            //   integer part      = 13
            //   fractional part   = round(0.0208 * 64) = 1
            self.write(IBRD, 13);
            self.write(FBRD, 1);

            self.write(LCR_H, LCR_H_FEN | LCR_H_WLEN_8);

            // Mask every interrupt and clear anything already pending. We are
            // polling for now; interrupts arrive in tier 1.
            self.write(IMSC, 0);
            self.write(ICR, 0x7ff);

            self.write(CR, CR_UARTEN | CR_TXE | CR_RXE);
        }
    }

    pub fn put_byte(&self, byte: u8) {
        unsafe {
            while self.read(FR) & FR_TXFF != 0 {
                core::hint::spin_loop();
            }
            self.write(DR, byte as u32);
        }
    }
}

impl fmt::Write for Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            // Terminals want CRLF; Rust gives us bare LF.
            if byte == b'\n' {
                self.put_byte(b'\r');
            }
            self.put_byte(byte);
        }
        Ok(())
    }
}

/// The console every `print!` in the kernel goes through.
///
/// Locked, because a `println!` is many separate writes to the data register
/// and an interrupt landing partway through would splice its own output into
/// the middle of the line. Garbled output during interrupt bring-up is worse
/// than no output: it makes you distrust the one instrument you have.
static CONSOLE: Lock<Uart> = Lock::new(unsafe { Uart::new(PL011_BASE) });

pub fn init() {
    CONSOLE.lock().init();
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use fmt::Write;
    // One lock for the whole format operation, not one per byte. The point is
    // that the entire line lands without interruption.
    let _ = CONSOLE.lock().write_fmt(args);
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::uart::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}
