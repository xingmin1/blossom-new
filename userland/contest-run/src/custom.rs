//! 内核测试通知设备（`/dev/testhook`）的单消息写入接口。
//!
//! 每个 case 自动发送 `before` 与 `after` 两条通知，消息只携带测例 ID，由内核按 ID 自行匹配
//! 需要特别处理的目标；未知 ID 在内核侧为无操作，因此普通测例不需要额外配置。通知设备不存在
//! 表示 BLOSSOM 组成不匹配，此时直接返回错误，不静默跳过本次会话的通知。
//!
//! 消息格式为 `before <ID>\n` 或 `after <ID>\n`，ID 不含空白与 NUL，整条消息不超过 512 字节。
//! 每次调用独立以 `O_WRONLY | O_CLOEXEC` 打开设备并只写一次；内核操作可能非幂等，出现短写即报错，
//! 不重发已经部分提交的消息。

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// 向指定路径的内核测试通知设备写入单条 `before` 或 `after` 消息。
///
/// 调用上下文：由测例执行编排在生命周期边界同步调用。`before` 为 `true` 表示测例开始，
/// 为 `false` 表示测例结束；`id` 为不含空白与 NUL 的测例标识符。
/// 返回值：单次完整写入成功返回 `Ok(())`；ID 非法、消息超过 512 字节、打开设备失败或发生短写时
/// 返回 `io::Error`，且不会自动重发。
pub fn notify_device(path: &Path, before: bool, id: &str) -> io::Result<()> {
    // Step 1: 校验测例 ID，避免破坏单行空格分隔的消息协议。
    if id.is_empty()
        || id
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_whitespace())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid case ID",
        ));
    }
    // Step 2: 组装单行消息并检查 512 字节长度上限。
    let phase = if before { "before" } else { "after" };
    let message = format!("{phase} {id}\n");
    if 512 < message.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "case notification exceeds 512 bytes",
        ));
    }
    // Step 3: 打开设备并单次写入；写入字节数不完整时直接报错，不重发可能已生效的内核操作。
    let mut device = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)?;
    let written = device.write(message.as_bytes())?;
    if written != message.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            format!(
                "short notification write: {written}/{}; message not replayed",
                message.len()
            ),
        ));
    }
    Ok(())
}

/// 向默认的 `/dev/testhook` 内核测试通知设备发送单条 `before` 或 `after` 消息。
///
/// 调用上下文：作为测例执行编排的默认通知回调传入；参数与返回值语义同 [`notify_device`]。
pub fn notify(before: bool, id: &str) -> io::Result<()> {
    notify_device(Path::new("/dev/testhook"), before, id)
}
