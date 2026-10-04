use core::fmt;
use core::ops;
use gimli::{LoongArch, Register};

use super::maybe_cfi;

// LoongArch64 的 GCC DWARF_FRAME_REGISTERS 为 74。
pub const MAX_REG_RULES: usize = 74;

// 两种目标分别使用 LP64D 与 LP64S，不能按内核运行时 FPE 状态更改目标 ABI。
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
            fmt.field(
                LoongArch::register_name(Register(i as _)).unwrap(),
                &self.gp[i],
            );
        }
        #[cfg(target_feature = "d")]
        for i in 0..=31 {
            fmt.field(
                LoongArch::register_name(Register((i + 32) as _)).unwrap(),
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
        "ld.d $tp, $a0, 0x10"
    };
}

macro_rules! code {
    (initialize) => {
        "
        move $t1, $sp
        addi.d $t2, $sp, {context_bytes}
        2:
        st.d $zero, $t1, 0
        addi.d $t1, $t1, 8
        bltu $t1, $t2, 2b
        "
    };
    (save_gp) => {
        "
        st.d $zero, $sp, 0x0
        st.d $ra, $sp, 0x8
        st.d $tp, $sp, 0x10
        st.d $t0, $sp, 0x18
        st.d $r21, $sp, 0xa8
        st.d $fp, $sp, 0xb0
        st.d $s0, $sp, 0xb8
        st.d $s1, $sp, 0xc0
        st.d $s2, $sp, 0xc8
        st.d $s3, $sp, 0xd0
        st.d $s4, $sp, 0xd8
        st.d $s5, $sp, 0xe0
        st.d $s6, $sp, 0xe8
        st.d $s7, $sp, 0xf0
        st.d $s8, $sp, 0xf8
        "
    };
    (save_fp) => {
        "
        fst.d $fs0, $sp, 0x1c0
        fst.d $fs1, $sp, 0x1c8
        fst.d $fs2, $sp, 0x1d0
        fst.d $fs3, $sp, 0x1d8
        fst.d $fs4, $sp, 0x1e0
        fst.d $fs5, $sp, 0x1e8
        fst.d $fs6, $sp, 0x1f0
        fst.d $fs7, $sp, 0x1f8
        "
    };
    (restore_gp) => {
        "
        ld.d $ra, $a0, 0x8
        ld.d $sp, $a0, 0x18
        ld.d $a1, $a0, 0x28
        ld.d $a2, $a0, 0x30
        ld.d $a3, $a0, 0x38
        ld.d $a4, $a0, 0x40
        ld.d $a5, $a0, 0x48
        ld.d $a6, $a0, 0x50
        ld.d $a7, $a0, 0x58
        ld.d $t0, $a0, 0x60
        ld.d $t1, $a0, 0x68
        ld.d $t2, $a0, 0x70
        ld.d $t3, $a0, 0x78
        ld.d $t4, $a0, 0x80
        ld.d $t5, $a0, 0x88
        ld.d $t6, $a0, 0x90
        ld.d $t7, $a0, 0x98
        ld.d $t8, $a0, 0xa0
        ld.d $r21, $a0, 0xa8
        ld.d $fp, $a0, 0xb0
        ld.d $s0, $a0, 0xb8
        ld.d $s1, $a0, 0xc0
        ld.d $s2, $a0, 0xc8
        ld.d $s3, $a0, 0xd0
        ld.d $s4, $a0, 0xd8
        ld.d $s5, $a0, 0xe0
        ld.d $s6, $a0, 0xe8
        ld.d $s7, $a0, 0xf0
        ld.d $s8, $a0, 0xf8
        "
    };
    (restore_fp) => {
        "
        fld.d $fa0, $a0, 0x100
        fld.d $fa1, $a0, 0x108
        fld.d $fa2, $a0, 0x110
        fld.d $fa3, $a0, 0x118
        fld.d $fa4, $a0, 0x120
        fld.d $fa5, $a0, 0x128
        fld.d $fa6, $a0, 0x130
        fld.d $fa7, $a0, 0x138
        fld.d $ft0, $a0, 0x140
        fld.d $ft1, $a0, 0x148
        fld.d $ft2, $a0, 0x150
        fld.d $ft3, $a0, 0x158
        fld.d $ft4, $a0, 0x160
        fld.d $ft5, $a0, 0x168
        fld.d $ft6, $a0, 0x170
        fld.d $ft7, $a0, 0x178
        fld.d $ft8, $a0, 0x180
        fld.d $ft9, $a0, 0x188
        fld.d $ft10, $a0, 0x190
        fld.d $ft11, $a0, 0x198
        fld.d $ft12, $a0, 0x1a0
        fld.d $ft13, $a0, 0x1a8
        fld.d $ft14, $a0, 0x1b0
        fld.d $ft15, $a0, 0x1b8
        fld.d $fs0, $a0, 0x1c0
        fld.d $fs1, $a0, 0x1c8
        fld.d $fs2, $a0, 0x1d0
        fld.d $fs3, $a0, 0x1d8
        fld.d $fs4, $a0, 0x1e0
        fld.d $fs5, $a0, 0x1e8
        fld.d $fs6, $a0, 0x1f0
        fld.d $fs7, $a0, 0x1f8
        "
    };
}

/// 保存 LP64D/LP64S 被调用者现场，Context 的 caller-saved 空槽先完整置零。
/// CFI 使用 DWARF 数字寄存器，避免依赖 LLVM 20 新增的 LA CFI 名字解析。
#[naked]
pub extern "C-unwind" fn save_context(f: extern "C" fn(&mut Context, *mut ()), ptr: *mut ()) {
    #[cfg(target_feature = "d")]
    // SAFETY: naked 入口保持 16 字节栈对齐，完整初始化 Context 后才交给 Rust，
    // RA 保存槽与 CFI 一致；调用者负责预先打开目标 ABI 需要的 FPE。
    unsafe {
        core::arch::naked_asm!(
            maybe_cfi!(".cfi_startproc"),
            "
        move $t0, $sp
        addi.d $sp, $sp, -0x210
        ",
            maybe_cfi!(".cfi_def_cfa_offset 0x210"),
            "
        st.d $ra, $sp, 0x200
        ",
            maybe_cfi!(".cfi_offset 1, -16"),
            code!(initialize),
            code!(save_gp),
            code!(save_fp),
            "
        move $t0, $a0
        move $a0, $sp
        jirl $ra, $t0, 0
        ld.d $ra, $sp, 0x200
        addi.d $sp, $sp, 0x210
        ",
            maybe_cfi!(".cfi_def_cfa_offset 0"),
            maybe_cfi!(".cfi_restore 1"),
            "ret",
            maybe_cfi!(".cfi_endproc"),
            context_bytes = const core::mem::size_of::<Context>(),
        );
    }
    #[cfg(not(target_feature = "d"))]
    // SAFETY: LP64S 分配 0x110 字节对齐帧；初始化不覆盖帧末尾 RA 保存槽。
    unsafe {
        core::arch::naked_asm!(
            maybe_cfi!(".cfi_startproc"),
            "
        move $t0, $sp
        addi.d $sp, $sp, -0x110
        ",
            maybe_cfi!(".cfi_def_cfa_offset 0x110"),
            "
        st.d $ra, $sp, 0x100
        ",
            maybe_cfi!(".cfi_offset 1, -16"),
            code!(initialize),
            code!(save_gp),
            "
        move $t0, $a0
        move $a0, $sp
        jirl $ra, $t0, 0
        ld.d $ra, $sp, 0x100
        addi.d $sp, $sp, 0x110
        ",
            maybe_cfi!(".cfi_def_cfa_offset 0"),
            maybe_cfi!(".cfi_restore 1"),
            "ret",
            maybe_cfi!(".cfi_endproc"),
            context_bytes = const core::mem::size_of::<Context>(),
        );
    }
}

/// 按 personality 给出的真实现场进入同栈 landing pad，内核不恢复 tp 或 KS0。
///
/// # Safety
/// ctx 必须完整初始化，SP/RA 指向当前有效栈及 Rust landing pad，LP64D 须已启用 FPE；
/// 展开期间不得调度或跨 trap、切栈汇编、普通 C ABI。
pub unsafe fn restore_context(ctx: &Context) -> ! {
    #[cfg(target_feature = "d")]
    // SAFETY: 调用者保证有效同栈现场和已开启的浮点单元，所有寄存器槽已初始化。
    unsafe {
        core::arch::asm!(
            code!(restore_fp),
            code!(restore_gp),
            restore_tp!(),
            "
            ld.d $a0, $a0, 0x20
            ret
            ",
            in("$r4") ctx,
            options(noreturn)
        );
    }
    #[cfg(not(target_feature = "d"))]
    // SAFETY: LP64S 不访问浮点寄存器，调用者保证合法栈及 landing pad。
    unsafe {
        core::arch::asm!(
            code!(restore_gp),
            restore_tp!(),
            "
            ld.d $a0, $a0, 0x20
            ret
            ",
            in("$r4") ctx,
            options(noreturn)
        );
    }
}
