//! 实际挂载拥有、系统根准备与不改变输入的有限 ELF 诊断。

use crate::execute::run_steps;
use crate::model::{CommandSpec, RootSpec};
use std::collections::{BTreeMap, VecDeque};
use std::ffi::{CString, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

pub const CONTEST_ROOT: &str = "/mnt/contest";

#[derive(Default)]
pub struct Mounts {
    owned: Vec<PathBuf>,
}

impl Mounts {
    /// 只有真实 mount 成功后才登记卸载责任。
    pub fn mount(
        &mut self,
        source: &Path,
        target: &Path,
        filesystem: &str,
        readonly: bool,
    ) -> io::Result<()> {
        fs::create_dir_all(target)?;
        let source_c = CString::new(source.as_os_str().as_bytes())?;
        let target_c = CString::new(target.as_os_str().as_bytes())?;
        let filesystem = CString::new(filesystem)?;
        let flags = if readonly { libc::MS_RDONLY } else { 0 };
        // SAFETY: 三个 CString 在调用期间保活且 NUL 终止；没有额外挂载参数。
        let result = unsafe {
            libc::mount(
                source_c.as_ptr(),
                target_c.as_ptr(),
                filesystem.as_ptr(),
                flags,
                std::ptr::null(),
            )
        };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        self.owned.push(target.to_path_buf());
        Ok(())
    }

    /// 逆序释放本实例取得的挂载，失败时保留剩余拥有且停止切换根。
    pub fn release(&mut self) -> Result<(), String> {
        while let Some(target) = self.owned.last() {
            let target_c =
                CString::new(target.as_os_str().as_bytes()).map_err(|error| error.to_string())?;
            // SAFETY: CString 在调用期间有效，仅普通卸载，不强制或 lazy 丢弃责任。
            if unsafe { libc::umount(target_c.as_ptr()) } == -1 {
                return Err(format!(
                    "unmount {}: {}",
                    target.display(),
                    io::Error::last_os_error()
                ));
            }
            self.owned.pop();
        }
        Ok(())
    }

    /// 系统根命令先执行；只挂载配置明确请求的 dev/proc/tmp。
    pub fn prepare_root(
        &mut self,
        root: &RootSpec,
        timeout: Duration,
        outputs: [RawFd; 2],
    ) -> Result<(), String> {
        if !root.path.is_dir() {
            return Err(format!("root is not a directory: {}", root.path.display()));
        }
        let errors = run_steps(
            None,
            Path::new("/"),
            &root.prepare,
            &BTreeMap::new(),
            timeout,
            outputs,
            true,
        );
        if let Some(error) = errors.into_iter().next() {
            return Err(format!("root prepare: {error}"));
        }
        for name in &root.mounts {
            let filesystem = match name.as_str() {
                "dev" => "devfs",
                "proc" => "proc",
                "tmp" => "tmpfs",
                _ => return Err(format!("unsupported root mount: {name}")),
            };
            let target = rooted_path(&root.path, Path::new("/"), Path::new(name))
                .map_err(|error| format!("mount target {name}: {error}"))?;
            self.mount(Path::new(filesystem), &target, filesystem, false)
                .map_err(|error| format!("mount {}: {error}", target.display()))?;
        }
        Ok(())
    }
}

/// 将路径逐组件限制在目标根；绝对 symlink 从目标根重新开始，最多跟随四十次。
pub(crate) fn rooted_path(root: &Path, cwd: &Path, path: &Path) -> io::Result<PathBuf> {
    let initial = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut pending: VecDeque<OsString> = initial
        .components()
        .map(|part| part.as_os_str().to_os_string())
        .collect();
    let mut resolved = PathBuf::new();
    let mut links = 0;
    while let Some(part) = pending.pop_front() {
        match Path::new(&part).components().next() {
            Some(Component::RootDir) => resolved.clear(),
            Some(Component::CurDir) | None => {}
            Some(Component::ParentDir) => {
                resolved.pop();
            }
            Some(Component::Normal(name)) => {
                resolved.push(name);
                let system_path = root.join(&resolved);
                match fs::symlink_metadata(&system_path) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        links += 1;
                        if 40 < links {
                            return Err(io::Error::from_raw_os_error(libc::ELOOP));
                        }
                        let target = fs::read_link(system_path)?;
                        resolved.pop();
                        for part in target.components().rev() {
                            pending.push_front(part.as_os_str().to_os_string());
                        }
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unsupported path prefix",
                ))
            }
        }
    }
    Ok(root.join(resolved))
}

/// 读取 ELF64 小端的有界 PT_INTERP，不遍历 DSO 或预测 exec 结果。
fn interpreter(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let mut file = File::open(path)?;
    let mut header = [0u8; 64];
    let count = file.read(&mut header)?;
    if count < 4 || &header[..4] != b"\x7fELF" {
        return Ok(None);
    }
    if count < header.len() || header[4] != 2 || header[5] != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "diagnostic supports ELF64 little-endian only",
        ));
    }
    let offset = u64::from_le_bytes(header[32..40].try_into().unwrap());
    let size = u16::from_le_bytes(header[54..56].try_into().unwrap()) as u64;
    let count = u16::from_le_bytes(header[56..58].try_into().unwrap()) as u64;
    if size < 56 || 128 < count {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "program-header diagnostic limit exceeded",
        ));
    }
    for index in 0..count {
        let entry = offset.checked_add(index * size).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "program-header offset overflow")
        })?;
        file.seek(SeekFrom::Start(entry))?;
        let mut ph = [0u8; 56];
        file.read_exact(&mut ph)?;
        if u32::from_le_bytes(ph[..4].try_into().unwrap()) != 3 {
            continue;
        }
        let offset = u64::from_le_bytes(ph[8..16].try_into().unwrap());
        let size = u64::from_le_bytes(ph[32..40].try_into().unwrap());
        if size < 2 || 4096 < size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "PT_INTERP diagnostic limit exceeded",
            ));
        }
        let mut bytes = vec![0; size as usize];
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut bytes)?;
        if bytes.pop() != Some(0) || bytes.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid PT_INTERP terminator",
            ));
        }
        return Ok(Some(bytes));
    }
    Ok(None)
}

/// 仅输出实际可读取的程序头信息，失败诊断不替换真实 spawn errno。
pub fn diagnose(root: &RootSpec, cwd: &Path, command: &CommandSpec) -> String {
    let program = match command {
        CommandSpec::Exec { program, .. } => program,
        CommandSpec::Shell { shell, .. } => shell,
    };
    if !program.contains('/') {
        return "PT_INTERP unavailable: program requires child PATH lookup".into();
    }
    let result =
        rooted_path(&root.path, cwd, Path::new(program)).and_then(|path| interpreter(&path));
    match result {
        Ok(None) => "PT_INTERP none (ELF static or non-ELF input)".into(),
        Ok(Some(bytes)) => {
            let interp = Path::new(std::ffi::OsStr::from_bytes(&bytes));
            let availability =
                rooted_path(&root.path, Path::new("/"), interp).and_then(fs::metadata);
            match availability {
                Ok(_) => format!("PT_INTERP {:?} present in target root", interp),
                Err(error) => format!("PT_INTERP {:?}: {error}", interp),
            }
        }
        Err(error) => format!("PT_INTERP diagnostic unavailable: {error}"),
    }
}
