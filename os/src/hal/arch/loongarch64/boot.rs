use core::arch::asm;
use core::arch::naked_asm;

use crate::config::KERNEL_STACK_SIZE;

#[naked]
#[no_mangle]
#[link_section = ".text.entry"]
unsafe extern "C" fn _start() -> ! {
    naked_asm!(
        r"
        ori         $t0, $zero, 0x1
        #lu52i.d     $t0, $t0, -2048
        csrwr       $t0, 0x180

        ori         $t0, $zero, 0x11
        #lu52i.d     $t0, $t0, -1792
        csrwr       $t0, 0x181

        # 设置栈
        la.global   $sp, {boot_stack}
        li.d        $t0, {boot_stack_size}
        add.d       $sp, $sp, $t0

        # 跳转到 rust_main
        la.global   $t0, {entry}
        jirl        $zero, $t0, 0x0
        ",
        boot_stack_size = const BOOT_STACK_SIZE,
        boot_stack = sym BOOT_STACK,
        entry = sym crate::rust_main
    )
}

pub const BOOT_STACK_SIZE: usize = KERNEL_STACK_SIZE;
#[link_section = ".bss.stack"]
pub(crate) static mut BOOT_STACK: [u8; BOOT_STACK_SIZE] = [0; BOOT_STACK_SIZE];