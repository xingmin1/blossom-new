//! 比赛控制者在普通可睡眠 Task 中调用具体操作；全局追踪覆盖同一时段的后台任务。

use crate::fs::FsError;
use crate::trace;

/// 对入口已校验的 case ID 执行前置操作；调用者不得持用户访问借用或子系统锁。
/// 追踪已开启时保留原 Busy 错误，合法未知 ID 不改变内核状态。
pub(crate) fn before(case: &[u8]) -> Result<(), FsError> {
    match case {
        b"runner:trace" => trace::start(),
        _ => Ok(()),
    }
}

/// 在控制者普通 Task 中结束具体操作；追踪停止会等待在线 CPU 的真实提交尾部。
/// 重复停止沿原接口成功，不清空已冻结记录，也不恢复其它子系统状态。
pub(crate) fn after(case: &[u8]) -> Result<(), FsError> {
    match case {
        b"runner:trace" => trace::stop(),
        _ => Ok(()),
    }
}
