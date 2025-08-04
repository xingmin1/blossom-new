#![allow(unused)]

use embedded_hal::serial::nb::{Read, Write};

use crate::drivers::Ns16550a;
use core::{arch::asm, mem::MaybeUninit};

use super::acpi::Pm1Cnt;
use super::board::UART_BASE;

pub static mut UART: Ns16550a = Ns16550a { base: UART_BASE };

pub fn console_putchar(c: usize) {
    let mut retry = 0;
    unsafe {
        UART.write(c as u8);
    }
}

pub fn console_flush() {
    unsafe { while UART.flush().is_err() {} }
}

pub fn console_getchar() -> usize {
    unsafe {
        if let Ok(i) = UART.read() {
            return i as usize;
        } else {
            return 1usize.wrapping_neg();
        }
    }
}

pub fn shutdown() -> ! {
    let mut pm1_cnt: Pm1Cnt = Pm1Cnt::empty();
    #[cfg(feature="board_2k1000")]
    pm1_cnt.set_s5().write();
    #[cfg(feature="board_laqemu")]
    {
        unsafe {
            (0x100E_001C as *mut u8).write_volatile(0x34);
        }
    }
    loop {}
}