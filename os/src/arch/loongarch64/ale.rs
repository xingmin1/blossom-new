//! 用户非对齐访存的纯解码与逐字节模拟，不访问真实用户虚拟指针。

use crate::arch::UserContext;
use crate::mm::space::VmError;

/// 模拟成功才提交现场；不支持的指令与用户访存错误分别由异常层选择信号。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EmulationResult {
    Emulated,
    Unsupported,
    Fault,
}

/// 字节访问可以处理缺页；错误终止本次模拟，已写入的前缀不回滚。
pub(crate) trait UserMemory {
    /// 读取一个用户字节，保留原地址空间错误。
    fn read_byte(&mut self, address: usize) -> Result<u8, VmError>;
    /// 在写权限及 COW 处理完成后写入一个用户字节。
    fn write_byte(&mut self, address: usize, value: u8) -> Result<(), VmError>;
}

pub(crate) struct Instruction {
    register: usize,
    width: usize,
    signed: bool,
    write: bool,
    floating: bool,
}

impl Instruction {
    /// 识别立即数、指针及索引访存编码，地址直接采用异常保存的 BADV。
    pub(crate) fn decode(word: u32) -> Option<Self> {
        let (width, signed, write, floating) = match word >> 22 {
            0xa1 => (2, true, false, false),
            0xa2 => (4, true, false, false),
            0xa3 => (8, true, false, false),
            0xa5 => (2, false, true, false),
            0xa6 => (4, false, true, false),
            0xa7 => (8, false, true, false),
            0xa9 => (2, false, false, false),
            0xaa => (4, false, false, false),
            0xac => (4, true, false, true),
            0xad => (4, false, true, true),
            0xae => (8, false, false, true),
            0xaf => (8, false, true, true),
            _ => match word >> 24 {
                0x24 => (4, true, false, false),
                0x25 => (4, false, true, false),
                0x26 => (8, true, false, false),
                0x27 => (8, false, true, false),
                _ => match word >> 15 {
                    0x7008 => (2, true, false, false),
                    0x7010 => (4, true, false, false),
                    0x7018 => (8, true, false, false),
                    0x7028 => (2, false, true, false),
                    0x7030 => (4, false, true, false),
                    0x7038 => (8, false, true, false),
                    0x7048 => (2, false, false, false),
                    0x7050 => (4, false, false, false),
                    0x7060 => (4, true, false, true),
                    0x7068 => (8, false, false, true),
                    0x7070 => (4, false, true, true),
                    0x7078 => (8, false, true, true),
                    _ => return None,
                },
            },
        };
        Some(Self {
            register: (word & 0x1f) as usize,
            width,
            signed,
            write,
            floating,
        })
    }

    /// 在短暂现场借用中取得存储源；整数零寄存器不依赖保存槽的内容。
    pub(crate) fn source_value(&self, context: &UserContext) -> u64 {
        if !self.write {
            0
        } else if self.floating {
            context.fpr[self.register].lanes[0]
        } else if self.register == 0 {
            0
        } else {
            context.gpr[self.register] as u64
        }
    }

    /// 在无现场借用的普通任务上下文访问各字节，允许缺页睡眠且不提供原子性。
    pub(crate) fn access(
        &self,
        memory: &mut impl UserMemory,
        address: usize,
        source: u64,
    ) -> Result<u64, VmError> {
        address
            .checked_add(self.width - 1)
            .ok_or(VmError::InvalidRange)?;
        let mut value = 0;
        for offset in 0..self.width {
            if self.write {
                memory.write_byte(address + offset, (source >> (offset * 8)) as u8)?;
            } else {
                value |= (memory.read_byte(address + offset)? as u64) << (offset * 8);
            }
        }
        Ok(value)
    }

    /// 全部访问成功后扩展加载结果并写回现场；浮点加载覆盖最低 64 位槽。
    pub(crate) fn complete(&self, context: &mut UserContext, value: u64) {
        if !self.write {
            let shift = (8 - self.width) * 8;
            let value = if self.signed {
                (((value << shift) as i64) >> shift) as u64
            } else {
                value
            };
            if self.floating {
                context.fpr[self.register].lanes[0] = value;
            } else if self.register != 0 {
                context.gpr[self.register] = value as usize;
            }
        }
        context.pc = context.pc.wrapping_add(4);
    }
}
