use core::fmt;
use core::ops;
use gimli::{Register, RiscV};

use super::maybe_cfi;

// Match DWARF_FRAME_REGISTERS in libgcc
pub const MAX_REG_RULES: usize = 65;

#[cfg(all(target_feature = "f", not(target_feature = "d")))]
compile_error!("RISC-V with only F extension is not supported");

#[repr(C)]
#[derive(Clone, Default)]
pub struct Context {
    pub gp: [usize; 32],
    #[cfg(target_feature = "d")]
    pub fp: [usize; 32],
}

impl fmt::Debug for Context {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut fmt = fmt.debug_struct("Context");
        for i in 0..=31 {
            fmt.field(RiscV::register_name(Register(i as _)).unwrap(), &self.gp[i]);
        }
        #[cfg(target_feature = "d")]
        for i in 0..=31 {
            fmt.field(
                RiscV::register_name(Register((i + 32) as _)).unwrap(),
                &self.fp[i],
            );
        }
        fmt.finish()
    }
}

impl ops::Index<Register> for Context {
    type Output = usize;

    fn index(&self, reg: Register) -> &usize {
        match reg {
            Register(0..=31) => &self.gp[reg.0 as usize],
            #[cfg(target_feature = "d")]
            Register(32..=63) => &self.fp[(reg.0 - 32) as usize],
            _ => unimplemented!(),
        }
    }
}

impl ops::IndexMut<gimli::Register> for Context {
    fn index_mut(&mut self, reg: Register) -> &mut usize {
        match reg {
            Register(0..=31) => &mut self.gp[reg.0 as usize],
            #[cfg(target_feature = "d")]
            Register(32..=63) => &mut self.fp[(reg.0 - 32) as usize],
            _ => unimplemented!(),
        }
    }
}

#[cfg(feature = "kernel-percpu")]
macro_rules! restore_tp {
    () => {
        ""
    };
}
#[cfg(not(feature = "kernel-percpu"))]
macro_rules! restore_tp {
    () => {
        "ld tp, 0x20(a0)"
    };
}

macro_rules! code {
    (initialize) => {
        "
        mv t1, sp
        addi t2, sp, {context_bytes}
        2:
        sd zero, 0(t1)
        addi t1, t1, 8
        bltu t1, t2, 2b
        "
    };
    (save_gp) => {
        "
        sd x0, 0x00(sp)
        sd ra, 0x08(sp)
        sd t0, 0x10(sp)
        sd gp, 0x18(sp)
        sd tp, 0x20(sp)
        sd s0, 0x40(sp)
        sd s1, 0x48(sp)
        sd s2, 0x90(sp)
        sd s3, 0x98(sp)
        sd s4, 0xA0(sp)
        sd s5, 0xA8(sp)
        sd s6, 0xB0(sp)
        sd s7, 0xB8(sp)
        sd s8, 0xC0(sp)
        sd s9, 0xC8(sp)
        sd s10, 0xD0(sp)
        sd s11, 0xD8(sp)
        "
    };
    (save_fp) => {
        // arch option manipulation needed due to LLVM/Rust bug, see rust-lang/rust#80608
        "
        .option push
        .option arch, +d
        fsd fs0, 0x140(sp)
        fsd fs1, 0x148(sp)
        fsd fs2, 0x190(sp)
        fsd fs3, 0x198(sp)
        fsd fs4, 0x1A0(sp)
        fsd fs5, 0x1A8(sp)
        fsd fs6, 0x1B0(sp)
        fsd fs7, 0x1B8(sp)
        fsd fs8, 0x1C0(sp)
        fsd fs9, 0x1C8(sp)
        fsd fs10, 0x1D0(sp)
        fsd fs11, 0x1D8(sp)
        .option pop
        "
    };
    (restore_gp) => {
        "
        ld ra, 0x08(a0)
        ld sp, 0x10(a0)
        ld gp, 0x18(a0)
        ld t0, 0x28(a0)
        ld t1, 0x30(a0)
        ld t2, 0x38(a0)
        ld s0, 0x40(a0)
        ld s1, 0x48(a0)
        ld a1, 0x58(a0)
        ld a2, 0x60(a0)
        ld a3, 0x68(a0)
        ld a4, 0x70(a0)
        ld a5, 0x78(a0)
        ld a6, 0x80(a0)
        ld a7, 0x88(a0)
        ld s2, 0x90(a0)
        ld s3, 0x98(a0)
        ld s4, 0xA0(a0)
        ld s5, 0xA8(a0)
        ld s6, 0xB0(a0)
        ld s7, 0xB8(a0)
        ld s8, 0xC0(a0)
        ld s9, 0xC8(a0)
        ld s10, 0xD0(a0)
        ld s11, 0xD8(a0)
        ld t3, 0xE0(a0)
        ld t4, 0xE8(a0)
        ld t5, 0xF0(a0)
        ld t6, 0xF8(a0)
        "
    };
    (restore_fp) => {
        "
        fld ft0, 0x100(a0)
        fld ft1, 0x108(a0)
        fld ft2, 0x110(a0)
        fld ft3, 0x118(a0)
        fld ft4, 0x120(a0)
        fld ft5, 0x128(a0)
        fld ft6, 0x130(a0)
        fld ft7, 0x138(a0)
        fld fs0, 0x140(a0)
        fld fs1, 0x148(a0)
        fld fa0, 0x150(a0)
        fld fa1, 0x158(a0)
        fld fa2, 0x160(a0)
        fld fa3, 0x168(a0)
        fld fa4, 0x170(a0)
        fld fa5, 0x178(a0)
        fld fa6, 0x180(a0)
        fld fa7, 0x188(a0)
        fld fs2, 0x190(a0)
        fld fs3, 0x198(a0)
        fld fs4, 0x1A0(a0)
        fld fs5, 0x1A8(a0)
        fld fs6, 0x1B0(a0)
        fld fs7, 0x1B8(a0)
        fld fs8, 0x1C0(a0)
        fld fs9, 0x1C8(a0)
        fld fs10, 0x1D0(a0)
        fld fs11, 0x1D8(a0)
        fld ft8, 0x1E0(a0)
        fld ft9, 0x1E8(a0)
        fld ft10, 0x1F0(a0)
        fld ft11, 0x1F8(a0)
        "
    };
}

/// 保存整数及目标 ABI 的浮点被调用者现场，随后调用只检查展开表的回调。
/// Context 的未保存 caller-saved 槽先置零，避免 Rust 克隆读取未初始化 usize。
#[naked]
pub extern "C-unwind" fn save_context(f: extern "C" fn(&mut Context, *mut ()), ptr: *mut ()) {
    // 本入口不跨调度，回调不得通过普通 C ABI 抛出第二个异常。
    #[cfg(target_feature = "d")]
    // SAFETY: naked 入口的栈遵循 16 字节 ABI；分配的 Context 完整初始化，
    // 仅使用 caller-saved 临时寄存器，保存的 RA 与 CFI 对应原调用者。
    unsafe {
        core::arch::naked_asm!(
            maybe_cfi!(".cfi_startproc"),
            "
            mv t0, sp
            add sp, sp, -0x210
            ",
            maybe_cfi!(".cfi_def_cfa_offset 0x210"),
            "sd ra, 0x200(sp)",
            maybe_cfi!(".cfi_offset ra, -16"),
            code!(initialize),
            code!(save_gp),
            code!(save_fp),
            "
            mv t0, a0
            mv a0, sp
            jalr t0
            ld ra, 0x200(sp)
            add sp, sp, 0x210
            ",
            maybe_cfi!(".cfi_def_cfa_offset 0"),
            maybe_cfi!(".cfi_restore ra"),
            "ret",
            maybe_cfi!(".cfi_endproc"),
            context_bytes = const core::mem::size_of::<Context>(),
        );
    }
    #[cfg(not(target_feature = "d"))]
    // SAFETY: 整数 ABI 使用 0x110 字节对齐帧，初始化长度不包含末尾 RA 保存槽。
    unsafe {
        core::arch::naked_asm!(
            maybe_cfi!(".cfi_startproc"),
            "
            mv t0, sp
            add sp, sp, -0x110
            ",
            maybe_cfi!(".cfi_def_cfa_offset 0x110"),
            "sd ra, 0x100(sp)",
            maybe_cfi!(".cfi_offset ra, -16"),
            code!(initialize),
            code!(save_gp),
            "
            mv t0, a0
            mv a0, sp
            jalr t0
            ld ra, 0x100(sp)
            add sp, sp, 0x110
            ",
            maybe_cfi!(".cfi_def_cfa_offset 0"),
            maybe_cfi!(".cfi_restore ra"),
            "ret",
            maybe_cfi!(".cfi_endproc"),
            context_bytes = const core::mem::size_of::<Context>(),
        );
    }
}

/// 从真实 DWARF 计算得到的现场进入 landing pad，不恢复内核的 per-CPU tp。
///
/// # Safety
/// ctx 必须完整初始化，SP/RA 属于当前仍存活的同一栈，FP 扩展须已启用；
/// 内核展开期间不得调度，也不得穿越 trap、切栈汇编或非展开 C ABI。
pub unsafe fn restore_context(ctx: &Context) -> ! {
    #[cfg(target_feature = "d")]
    // SAFETY: 调用者保证同栈合法 landing pad、对齐现场和已启用的 FP ABI。
    unsafe {
        core::arch::asm!(
            code!(restore_fp),
            code!(restore_gp),
            restore_tp!(),
            "
            ld a0, 0x50(a0)
            ret
            ",
            in("a0") ctx,
            options(noreturn)
        );
    }
    #[cfg(not(target_feature = "d"))]
    // SAFETY: 整数 ABI 不执行 FP 指令；调用者保证合法的栈和 landing pad。
    unsafe {
        core::arch::asm!(
            code!(restore_gp),
            restore_tp!(),
            "
            ld a0, 0x50(a0)
            ret
            ",
            in("a0") ctx,
            options(noreturn)
        );
    }
}
