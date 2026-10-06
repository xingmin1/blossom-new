//! 用户栈上的整数现场与单个 FPU 扩展记录；内核续接和特权 CSR 不进入用户帧。

use core::convert::TryInto;

use crate::arch::loongarch64::user::UserContext;
use crate::arch::{UserAccess, USER_ADDRESS_END};
use crate::mm::space::{Mm, VmError};
use crate::task::signal::{
    AltStack, SignalFrameArgs, SignalFrameRestore, SignalInfo, SignalPayload, SA_ONSTACK,
    SS_DISABLE, UNBLOCKABLE,
};

pub(crate) const SIGNAL_MIN_STACK: usize = 4096;

const INFO_SIZE: usize = 128;
const UCONTEXT: usize = INFO_SIZE;
const STACK: usize = UCONTEXT + 16;
const MASK: usize = UCONTEXT + 40;
const MCONTEXT: usize = UCONTEXT + 176;
const REGISTERS: usize = MCONTEXT + 8;
const FLAGS: usize = REGISTERS + 32 * 8;
const EXTCONTEXT: usize = MCONTEXT + 272;
const SCTX_INFO_SIZE: usize = 16;
const FPU_CONTEXT_SIZE: usize = 272;
const FPU_RECORD_SIZE: usize = SCTX_INFO_SIZE + FPU_CONTEXT_SIZE;
const FRAME_SIZE: usize = EXTCONTEXT + FPU_RECORD_SIZE + SCTX_INFO_SIZE;
const FPU_MAGIC: u32 = 0x4650_5501;
const SC_USED_FP: u32 = 1;
const SC_ADDRERR_RD: u32 = 1 << 30;
const SC_ADDRERR_WR: u32 = 1 << 31;

impl UserContext {
    /// 在普通任务中完整写入用户帧后改变 handler 现场；逐页 uaccess 可缺页或处理 COW。
    /// 调用者不得持有 siglock、IRQ guard 或用户现场借用外的短锁。
    pub(crate) fn setup_signal(&mut self, mm: &Mm, args: &SignalFrameArgs) -> Result<(), VmError> {
        let sp = frame_address(self.gpr[3], args)?;
        let mut bytes = [0u8; FRAME_SIZE];
        args.info.encode(&mut bytes[..INFO_SIZE]);
        put_u64(&mut bytes, STACK, args.altstack.base as u64);
        put_u32(&mut bytes, STACK + 8, args.altstack.flags);
        put_u64(&mut bytes, STACK + 16, args.altstack.size as u64);
        put_u64(&mut bytes, MASK, args.restore_mask);
        put_u64(&mut bytes, MCONTEXT, self.pc as u64);
        for (index, register) in self.gpr.iter().enumerate().skip(1) {
            put_u64(&mut bytes, REGISTERS + index * 8, *register as u64);
        }
        put_u32(&mut bytes, FLAGS, context_flags(args));
        put_u32(&mut bytes, EXTCONTEXT, FPU_MAGIC);
        put_u32(&mut bytes, EXTCONTEXT + 4, FPU_RECORD_SIZE as u32);
        let fpu = EXTCONTEXT + SCTX_INFO_SIZE;
        for (index, register) in self.fpr.iter().enumerate() {
            put_u64(&mut bytes, fpu + index * 8, *register);
        }
        put_u64(&mut bytes, fpu + 256, self.fcc as u64);
        put_u32(&mut bytes, fpu + 264, self.fcsr as u32);
        // 清零缓冲同时初始化所有 ABI padding、uc_flags/link 和最终 magic=0/size=0。
        let progress = mm.copyout(sp, &bytes);
        if let Some(error) = progress.error {
            return Err(error);
        }
        assert_eq!(
            progress.copied,
            bytes.len(),
            "successful signal frame copy is incomplete"
        );
        self.gpr[1] = args.trampoline;
        self.gpr[3] = sp;
        self.gpr[4] = args.info.signum as usize;
        self.gpr[5] = sp;
        self.gpr[6] = sp + UCONTEXT;
        self.pc = args.action.handler;
        Ok(())
    }

    /// 从当前用户 SP 解码完整候选；任何复制或记录错误都不修改当前现场及信号状态。
    /// 完整验证成功后若 FCSR 有已启用异常，先将清除 cause 的值回写用户帧；回写失败仍不提交候选。
    pub(crate) fn restore_signal(&self, mm: &Mm) -> Result<SignalFrameRestore, VmError> {
        let sp = self.gpr[3];
        if sp & 15 != 0 {
            return Err(VmError::InvalidRange);
        }
        let mut bytes = [0u8; EXTCONTEXT + SCTX_INFO_SIZE];
        copyin_exact(mm, sp, &mut bytes)?;
        let flags = get_u32(&bytes, FLAGS);
        if flags & !(SC_USED_FP | SC_ADDRERR_RD | SC_ADDRERR_WR) != 0 {
            return Err(VmError::InvalidRange);
        }
        let altstack = decode_altstack(&bytes);
        // new 清除异常 CSR 与 kernel_sp；恢复只写用户整数、PC 和浮点字段。
        let mut context = Self::new(get_u64(&bytes, MCONTEXT) as usize, 0);
        for (index, register) in context.gpr.iter_mut().enumerate().skip(1) {
            *register = get_u64(&bytes, REGISTERS + index * 8) as usize;
        }
        let magic = get_u32(&bytes, EXTCONTEXT);
        let size = get_u32(&bytes, EXTCONTEXT + 4) as usize;
        match magic {
            0 if size == 0 && flags & SC_USED_FP == 0 => {}
            FPU_MAGIC if FPU_RECORD_SIZE <= size && size & 15 == 0 => {
                let record = sp.checked_add(EXTCONTEXT).ok_or(VmError::InvalidRange)?;
                let end = record.checked_add(size).ok_or(VmError::InvalidRange)?;
                let mut fpu = [0u8; FPU_CONTEXT_SIZE];
                copyin_exact(mm, record + SCTX_INFO_SIZE, &mut fpu)?;
                let mut terminator = [0u8; SCTX_INFO_SIZE];
                copyin_exact(mm, end, &mut terminator)?;
                // 只有实际保存的 FPU 记录可恢复；重复 FPU、LSX/LASX/LBT 或未知 magic 均失败。
                if get_u32(&terminator, 0) != 0 || get_u32(&terminator, 4) != 0 {
                    return Err(VmError::InvalidRange);
                }
                for (index, register) in context.fpr.iter_mut().enumerate() {
                    *register = get_u64(&fpu, index * 8);
                }
                context.fcc = get_u64(&fpu, 256) as usize;
                context.fcsr = get_u32(&fpu, 264) as usize;
            }
            _ => return Err(VmError::InvalidRange),
        }
        let pending_signal = restore_fcsr_pending(mm, sp, &mut context)?;
        Ok(SignalFrameRestore {
            context,
            mask: get_u64(&bytes, MASK) & !UNBLOCKABLE,
            altstack,
            pending_signal,
        })
    }
}

/// 选择普通栈或备用栈顶部并检查整个含扩展记录的帧，不让嵌套帧越过备用栈底。
fn frame_address(sp: usize, args: &SignalFrameArgs) -> Result<usize, VmError> {
    let stack = args.altstack;
    let nested = stack.on_stack(sp);
    let use_altstack = args.action.flags & SA_ONSTACK != 0
        && stack.flags & SS_DISABLE == 0
        && stack.size != 0
        && !nested;
    let top = if use_altstack {
        stack
            .base
            .checked_add(stack.size)
            .ok_or(VmError::InvalidRange)?
    } else {
        sp
    };
    let address = (top & !15)
        .checked_sub(FRAME_SIZE)
        .ok_or(VmError::InvalidRange)?;
    if USER_ADDRESS_END < top || ((nested || use_altstack) && address <= stack.base) {
        return Err(VmError::InvalidRange);
    }
    Ok(address)
}

/// eager FPU 保存真实浮点记录；PageFault 的 trap 是公共访问判别，不是硬件异常号。
fn context_flags(args: &SignalFrameArgs) -> u32 {
    let direction = match args.info.payload {
        SignalPayload::Fault { trap, .. } if trap == UserAccess::Write as usize => SC_ADDRERR_WR,
        SignalPayload::Fault { trap, .. }
            if trap == UserAccess::Read as usize || trap == UserAccess::Execute as usize =>
        {
            SC_ADDRERR_RD
        }
        _ => 0,
    };
    SC_USED_FP | direction
}

/// 完整解码备用栈原始候选；是否可替换登记由信号状态所有者按恢复后的 SP 判断。
fn decode_altstack(bytes: &[u8]) -> AltStack {
    AltStack {
        base: get_u64(bytes, STACK) as usize,
        size: get_u64(bytes, STACK + 16) as usize,
        flags: get_u32(bytes, STACK + 8),
    }
}

/// 将帧中已启用的浮点异常 cause 清除并回写原槽，成功后请求内核生成的同步 SIGFPE。
fn restore_fcsr_pending(
    mm: &Mm,
    sp: usize,
    context: &mut UserContext,
) -> Result<Option<SignalInfo>, VmError> {
    let fcsr = context.fcsr as u32;
    let enabled = (fcsr & 0x1f) << 24;
    if fcsr & enabled == 0 {
        return Ok(None);
    }
    let cleared = fcsr & !enabled;
    let address = sp
        .checked_add(EXTCONTEXT + SCTX_INFO_SIZE + 264)
        .ok_or(VmError::InvalidRange)?;
    let progress = mm.copyout(address, &cleared.to_le_bytes());
    if let Some(error) = progress.error {
        return Err(error);
    }
    assert_eq!(
        progress.copied, 4,
        "successful signal FCSR copy is incomplete"
    );
    context.fcsr = cleared as usize;
    Ok(Some(SignalInfo::kernel(8)))
}

/// 完整复制后才允许解码；跨页、缺页和 COW 的访问责任由正式 Mm 入口承担。
fn copyin_exact(mm: &Mm, address: usize, bytes: &mut [u8]) -> Result<(), VmError> {
    let progress = mm.copyin(bytes, address);
    if let Some(error) = progress.error {
        return Err(error);
    }
    assert_eq!(
        progress.copied,
        bytes.len(),
        "successful signal frame copy is incomplete"
    );
    Ok(())
}

/// 在初始化缓冲中写入小端机器字，不借用用户对象或结构体 padding。
fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

/// 写入小端固定宽度标志或记录头字段。
fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

/// 仅从已完整复制且由调用者保证范围的字节缓冲解码机器字。
fn get_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("signal frame field range"),
    )
}

/// 仅从已完整复制且由调用者保证范围的字节缓冲解码固定宽度字段。
fn get_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("signal frame field range"),
    )
}
