//! 动作表独立于线程组身份；共享处理处置不合并不同进程的待递送信号。

use alloc::alloc::AllocError;
use alloc::sync::Arc;

use crate::sync::SpinLock;

pub(crate) const SIGNAL_COUNT: usize = 64;
pub(crate) const UNBLOCKABLE: u64 = (1 << (9 - 1)) | (1 << (19 - 1));
const ACTION_FLAGS: usize = 1 | 2 | 4 | 0x0800_0000 | 0x1000_0000 | 0x4000_0000 | 0x8000_0000;

/// 两目标的动作对象都没有restorer字段；内核只保存值，不解引用handler地址。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SignalAction {
    pub(crate) handler: usize,
    pub(crate) flags: usize,
    pub(crate) mask: u64,
}

impl SignalAction {
    pub(crate) const DEFAULT: Self = Self {
        handler: 0,
        flags: 0,
        mask: 0,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SignalError {
    InvalidSignal,
}

/// 同一动作锁串行取旧值和替换；用户对象复制与clone分配在短锁外。
pub(crate) struct Sighand {
    actions: SpinLock<[SignalAction; SIGNAL_COUNT]>,
}

impl Sighand {
    /// 锁外构造完整默认表，尚未绑定到任何可运行用户线程。
    pub(crate) fn try_new() -> Result<Arc<Self>, AllocError> {
        Arc::try_new(Self {
            actions: SpinLock::new([SignalAction::DEFAULT; SIGNAL_COUNT]),
        })
    }

    /// fork复制本次处置快照；新表分配不占原siglock，不复制任何pending实例。
    pub(crate) fn try_copy(&self) -> Result<Arc<Self>, AllocError> {
        let actions = *self.actions.lock();
        Arc::try_new(Self {
            actions: SpinLock::new(actions),
        })
    }

    /// 取得并按需替换同一动作；KILL/STOP只允许查询，未知标志清除以保能力探测语义。
    pub(crate) fn update(
        &self,
        signal: usize,
        action: Option<SignalAction>,
    ) -> Result<SignalAction, SignalError> {
        if signal == 0 || SIGNAL_COUNT < signal || (action.is_some() && matches!(signal, 9 | 19)) {
            return Err(SignalError::InvalidSignal);
        }
        let mut actions = self.actions.lock();
        let current = &mut actions[signal - 1];
        let previous = *current;
        if let Some(mut action) = action {
            action.flags &= ACTION_FLAGS;
            action.mask &= !UNBLOCKABLE;
            *current = action;
        }
        Ok(previous)
    }
}
