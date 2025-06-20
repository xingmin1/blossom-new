use crate::config::HIGH_BASE_EIGHT;

pub const MMIO: &[(usize, usize)] = &[
    (0x1fe0_01e0, 0x100), // QEMU中的串口 UART0
];

pub const BLOCK_SZ: usize = 4096;
// warning: 不能移除“ + HIGH_BASE_EIGHT”，会导致开发板上地址错误
pub const UART_BASE: usize = 0x1fe001e0;
pub const ACPI_BASE: usize = 0x1FE2_7000 + HIGH_BASE_EIGHT;
