//! 具体内核前后操作使用设备的单消息 write 契约。

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// 每次只提交一条完整消息；短写或 EINTR 都保留错误，不重放可能已生效的操作。
pub fn hook_device(path: &Path, before: bool, id: &str) -> io::Result<()> {
    if id.is_empty()
        || id
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_whitespace())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid hook case ID",
        ));
    }
    let operation = if before { "before" } else { "after" };
    let message = format!("{operation} {id}\n");
    if 512 < message.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "hook message exceeds 512 bytes",
        ));
    }
    let mut device = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)?;
    let written = device.write(message.as_bytes())?;
    if written != message.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            format!(
                "hook short write: {written}/{}; message not replayed",
                message.len()
            ),
        ));
    }
    Ok(())
}

/// runner:trace 与其它显式 Hook 都直接交现有 testhook 设备分派。
pub fn hook(before: bool, id: &str) -> io::Result<()> {
    hook_device(Path::new("/dev/testhook"), before, id)
}
