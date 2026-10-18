//! 磁盘主映像与真实 ELF 解释器在同一私有 Mm 中准备，文件整页同源，边缘页先初始化。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cmp::{max, min};
use core::convert::{TryFrom, TryInto};

use crate::arch::{self, UserContext};
use crate::block::BlockError;
use crate::fs::{
    self, ExecAccess, FileIoContext, FileRef, FsContext, FsError, InodeKind, ReadBuffer,
    ResolveMode, Vfs,
};
use crate::mm::space::{Mm, VmError};
use crate::mm::uaccess::CStringError;
use crate::mm::user_table::{PagePermissions, PageTableError};
use crate::mm::{alloc_pages, PAGE_SIZE};
use crate::random;
use crate::task::Credentials;

pub(crate) const MAX_ARGUMENT_BYTES: usize = 32768;
pub(crate) const MAX_ARGUMENT_COUNT: usize = 128;
const MAX_PATH_BYTES: usize = 4096;
pub(crate) const DEFAULT_STACK_BYTES: u64 = 8 * 1024 * 1024;
const LOAD_LIMIT: usize = arch::USER_STACK_TOP - DEFAULT_STACK_BYTES as usize - PAGE_SIZE;
const MAX_IMAGE_PAGES: usize = 16384;
const MAX_IMAGE_SPAN: usize = 256 * 1024 * 1024;
const ELF_HEADER_BYTES: usize = 64;
const SCRIPT_HEADER_BYTES: usize = 256;
const MAX_INTERPRETER_DEPTH: usize = 5;
const PHDR_BYTES: usize = 56;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const MAIN_DYN_BASE: usize = 0x2000_0000;
const INTERP_DYN_BASE: usize = 0x3000_0000;
const PT_PHDR: u32 = 6;
const PT_TLS: u32 = 7;
const PF_R: u32 = 4;
const PF_W: u32 = 2;
const PF_X: u32 = 1;
const RW: PagePermissions = PagePermissions {
    read: true,
    write: true,
    execute: false,
};
const RX: PagePermissions = PagePermissions {
    read: true,
    write: false,
    execute: true,
};
const RWX: PagePermissions = PagePermissions {
    read: true,
    write: true,
    execute: true,
};

/// 格式、参数预算和实际文件、VM、分配失败保持可区分。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExecError {
    Format,
    ArgumentsTooBig,
    TooManyInterpreters,
    NoMemory,
    File(FsError),
    Vm(VmError),
}

impl From<FsError> for ExecError {
    /// 文件层的具体错误不转换为格式错误或空数据。
    fn from(error: FsError) -> Self {
        Self::File(error)
    }
}

impl From<VmError> for ExecError {
    /// 保留 VM 所报告的权限、范围、文件及分配原因。
    fn from(error: VmError) -> Self {
        Self::Vm(error)
    }
}

impl From<PageTableError> for ExecError {
    /// 私有根和原页安装失败保持实际页表原因。
    fn from(error: PageTableError) -> Self {
        match error {
            PageTableError::NoMemory => Self::NoMemory,
            other => Self::Vm(other.into()),
        }
    }
}

/// 参数为一次拥有的原始字节快照，字符串不包含 NUL，不依赖 UTF-8。
pub(crate) struct Arguments {
    filename: Vec<u8>,
    argv: Vec<Vec<u8>>,
    envp: Vec<Vec<u8>>,
    bytes: usize,
}

impl Arguments {
    /// 拷贝内核提供的原始参数；总预算包含路径、终止符、argc 和全部参数指针。
    pub(crate) fn from_kernel(
        filename: &[u8],
        argv: &[&[u8]],
        envp: &[&[u8]],
    ) -> Result<Self, ExecError> {
        validate_filename(filename)?;
        let mut args = Self::new(copy_bytes(filename)?);
        for string in argv {
            args.push_kernel(string, true)?;
        }
        args.ensure_argv0()?;
        for string in envp {
            args.push_kernel(string, false)?;
        }
        Ok(args)
    }

    /// 每个用户指针和字符串只读取一次，后续映像准备仅访问拥有的快照。
    /// 并发用户写入不保证多字符串的原子快照，访问错误按原 VM 原因返回。
    pub(crate) fn from_user(
        mm: &Mm,
        filename: Vec<u8>,
        argv: usize,
        envp: usize,
    ) -> Result<Self, ExecError> {
        validate_filename(&filename)?;
        let mut args = Self::new(filename);
        args.read_vector(mm, argv, true)?;
        args.ensure_argv0()?;
        args.read_vector(mm, envp, false)?;
        Ok(args)
    }

    /// 原执行路径保持不变，解释器替换 argv[0] 不改变 AT_EXECFN。
    pub(crate) fn filename(&self) -> &[u8] {
        &self.filename
    }

    /// 先核完整预算并准备新增字符串和原向量容量，再一次移动前缀；失败保持原参数内容。
    fn prepend_interpreter(
        &mut self,
        interpreter: &[u8],
        optional: Option<&[u8]>,
        original: bool,
    ) -> Result<(), ExecError> {
        let script = if original {
            &self.filename
        } else {
            &self.argv[0]
        };
        let argc = self.argv.len() + 1 + usize::from(optional.is_some());
        if MAX_ARGUMENT_COUNT < argc + self.envp.len() {
            return Err(ExecError::ArgumentsTooBig);
        }
        let word = core::mem::size_of::<usize>();
        let old_bytes = self.bytes - self.argv[0].len() - 1 - word;
        let inserted = interpreter.len()
            + script.len()
            + 2 * (1 + word)
            + optional.map_or(0, |argument| argument.len() + 1 + word);
        let bytes = old_bytes
            .checked_add(inserted)
            .filter(|bytes| *bytes <= MAX_ARGUMENT_BYTES)
            .ok_or(ExecError::ArgumentsTooBig)?;
        let additional = argc - self.argv.len();
        let interpreter = copy_bytes(interpreter)?;
        let optional = optional.map(copy_bytes).transpose()?;
        let script = if original && self.argv[0] != self.filename {
            Some(copy_bytes(script)?)
        } else {
            None
        };

        self.argv
            .try_reserve_exact(additional)
            .map_err(|_| ExecError::NoMemory)?;
        let script = match script {
            Some(script) => script,
            None => core::mem::take(&mut self.argv[0]),
        };
        match optional {
            Some(optional) => drop(self.argv.splice(
                0..1,
                IntoIterator::into_iter([interpreter, optional, script]),
            )),
            None => drop(
                self.argv
                    .splice(0..1, IntoIterator::into_iter([interpreter, script])),
            ),
        }
        self.bytes = bytes;
        Ok(())
    }

    /// 输入已验证有界；移动路径并预留argc与两个终止指针的预算。
    fn new(filename: Vec<u8>) -> Self {
        let bytes = filename.len() + 1 + 3 * core::mem::size_of::<usize>();
        Self {
            filename,
            argv: Vec::new(),
            envp: Vec::new(),
            bytes,
        }
    }

    /// 参数与环境合计使用有限个指针，每项在分配或读取前取得剩余字节预算。
    fn string_limit(&self) -> Result<usize, ExecError> {
        if MAX_ARGUMENT_COUNT <= self.argv.len() + self.envp.len() {
            return Err(ExecError::ArgumentsTooBig);
        }
        let remaining = MAX_ARGUMENT_BYTES
            .checked_sub(self.bytes + core::mem::size_of::<usize>())
            .ok_or(ExecError::ArgumentsTooBig)?;
        if remaining == 0 {
            return Err(ExecError::ArgumentsTooBig);
        }
        Ok(remaining)
    }

    /// 先验证有界内核字节，再取得唯一字符串拥有，禁止隐式不可失败分配。
    fn push_kernel(&mut self, string: &[u8], argument: bool) -> Result<(), ExecError> {
        let limit = self.string_limit()?;
        if limit <= string.len() {
            return Err(ExecError::ArgumentsTooBig);
        }
        if string.contains(&0) {
            return Err(ExecError::Format);
        }
        let string = copy_bytes(string)?;
        self.push_owned(string, argument)
    }

    /// 只在真实预留成功后移动字符串，失败时候选字符串和此前快照正常析构。
    fn push_owned(&mut self, string: Vec<u8>, argument: bool) -> Result<(), ExecError> {
        let bytes = self
            .bytes
            .checked_add(string.len())
            .and_then(|bytes| bytes.checked_add(1 + core::mem::size_of::<usize>()))
            .filter(|bytes| *bytes <= MAX_ARGUMENT_BYTES)
            .ok_or(ExecError::ArgumentsTooBig)?;
        let vector = if argument {
            &mut self.argv
        } else {
            &mut self.envp
        };
        vector.try_reserve(1).map_err(|_| ExecError::NoMemory)?;
        self.bytes = bytes;
        vector.push(string);
        Ok(())
    }

    /// 空或 NULL argv 按初始栈 ABI 产生一个空 argv[0]。
    fn ensure_argv0(&mut self) -> Result<(), ExecError> {
        if self.argv.is_empty() {
            self.push_kernel(&[], true)?;
        }
        Ok(())
    }

    /// 从有界指针数组取得原字节；NULL 数组无需解引用，末项 NULL 也只读取一次。
    fn read_vector(&mut self, mm: &Mm, address: usize, argument: bool) -> Result<(), ExecError> {
        if address == 0 {
            return Ok(());
        }
        let mut slot = address;
        loop {
            let pointer = mm.read_usize(slot)?;
            if pointer == 0 {
                return Ok(());
            }
            let limit = self.string_limit()?;
            let string = mm
                .read_c_string(pointer, limit)
                .map_err(|error| match error {
                    CStringError::Vm(error) => ExecError::Vm(error),
                    CStringError::NoMemory => ExecError::NoMemory,
                    CStringError::Unterminated => ExecError::ArgumentsTooBig,
                })?;
            self.push_owned(string, argument)?;
            slot = slot
                .checked_add(core::mem::size_of::<usize>())
                .ok_or(ExecError::Vm(VmError::InvalidRange))?;
        }
    }
}

/// 路径在分配或移动前验证；空路径保留查找错误，内嵌 NUL 仍按格式错误拒绝。
fn validate_filename(filename: &[u8]) -> Result<(), ExecError> {
    if MAX_PATH_BYTES <= filename.len() {
        return Err(ExecError::ArgumentsTooBig);
    }
    if filename.is_empty() {
        return Err(ExecError::File(FsError::NotFound));
    }
    if filename.contains(&0) {
        return Err(ExecError::Format);
    }
    Ok(())
}

/// 只复制已经验证有界的字节，分配失败不触发内核不可恢复的 OOM 路径。
pub(crate) fn copy_bytes(bytes: &[u8]) -> Result<Vec<u8>, ExecError> {
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(bytes.len())
        .map_err(|_| ExecError::NoMemory)?;
    owned.extend_from_slice(bytes);
    Ok(owned)
}

pub(crate) struct LoadedImage {
    pub mm: Arc<Mm>,
    pub context: UserContext,
}

/// 在同一 root/cwd 和栈限额快照下准备 ELF 或脚本链，成功前不改变正在运行的映像。
pub(crate) fn prepare(
    vfs: &Vfs,
    context: &FsContext,
    mut args: Arguments,
    credentials: Credentials,
    stack_limit: u64,
) -> Result<LoadedImage, ExecError> {
    let (mut file, mut access) = open_executable(vfs, context, args.filename(), credentials)?;
    for depth in 0..=MAX_INTERPRETER_DEPTH {
        let mut header = [0; SCRIPT_HEADER_BYTES];
        read_prefix(&file, 0, &mut header[..ELF_HEADER_BYTES])?;
        if header[..2] != *b"#!" {
            return load_elf(
                vfs,
                context,
                file,
                access,
                (&header[..ELF_HEADER_BYTES]).try_into().unwrap(),
                &args,
                credentials,
                stack_limit,
            );
        }
        read_prefix(
            &file,
            ELF_HEADER_BYTES as u64,
            &mut header[ELF_HEADER_BYTES..],
        )?;
        let (interpreter, optional) = script_interpreter(&header)?;
        args.prepend_interpreter(interpreter, optional, depth == 0)?;
        let next = open_executable(vfs, context, &args.argv[0], credentials)?;
        // 新文件已经取得写排斥，接替后才放掉上一个脚本的文件与执行写占用。
        (file, access) = next;
    }
    Err(ExecError::TooManyInterpreters)
}

/// 每层解释器都使用同一凭据与路径规则，首读前取得真实普通 inode 的执行写占用。
fn open_executable(
    vfs: &Vfs,
    context: &FsContext,
    pathname: &[u8],
    credentials: Credentials,
) -> Result<(FileRef, ExecAccess), ExecError> {
    let path = vfs.resolve_from(
        context,
        context.cwd(),
        pathname,
        ResolveMode::Follow,
        Some((credentials.uid, credentials.gid)),
    )?;
    if path.inode()?.metadata_snapshot()?.kind != InodeKind::Regular {
        return Err(ExecError::File(FsError::Permission));
    }
    path.check_access(credentials.uid, credentials.gid, 1)?;
    let file = fs::open_path(vfs, path)?;
    let access = file
        .mapping_inode()
        .ok_or(ExecError::Format)?
        .deny_writes()?;
    Ok((file, access))
}

/// 只将空格和 tab 当作分隔符，拒绝截断的解释器路径，余下内容作为一个可选参数。
fn script_interpreter(
    header: &[u8; SCRIPT_HEADER_BYTES],
) -> Result<(&[u8], Option<&[u8]>), ExecError> {
    let newline = header
        .iter()
        .take_while(|byte| **byte != 0)
        .position(|byte| *byte == b'\n');
    let mut end = newline.unwrap_or(SCRIPT_HEADER_BYTES - 1);
    if newline.is_none() {
        let name = header[2..=end]
            .iter()
            .position(|byte| !matches!(*byte, b' ' | b'\t'))
            .map(|index| index + 2)
            .ok_or(ExecError::Format)?;
        if !header[name..=end]
            .iter()
            .any(|byte| matches!(*byte, b' ' | b'\t' | 0))
        {
            return Err(ExecError::Format);
        }
    }
    while 2 < end && matches!(header[end - 1], b' ' | b'\t') {
        end -= 1;
    }
    let start = header[2..=end]
        .iter()
        .position(|byte| !matches!(*byte, b' ' | b'\t'))
        .map(|index| index + 2)
        .filter(|start| *start < end)
        .ok_or(ExecError::Format)?;
    let separator = header[start..=end]
        .iter()
        .position(|byte| matches!(*byte, b' ' | b'\t' | 0))
        .map(|index| index + start);
    let name_end = separator.unwrap_or(end);
    let optional = separator
        .filter(|index| header[*index] != 0)
        .and_then(|separator| {
            header[separator..=end]
                .iter()
                .position(|byte| !matches!(*byte, b' ' | b'\t'))
                .map(|index| separator + index)
        })
        .map(|start| {
            let stop = header[start..end]
                .iter()
                .position(|byte| *byte == 0)
                .map_or(end, |index| start + index);
            &header[start..stop]
        });
    Ok((&header[start..name_end], optional))
}

/// 首读前取得主 inode 写排斥，按显式栈限额与同一文件系统快照准备，失败归还候选拥有。
pub(crate) fn load(
    vfs: &Vfs,
    context: &FsContext,
    file: FileRef,
    args: &Arguments,
    credentials: Credentials,
    stack_limit: u64,
) -> Result<LoadedImage, ExecError> {
    let access = file
        .mapping_inode()
        .ok_or(ExecError::Format)?
        .deny_writes()?;
    let mut header = [0; ELF_HEADER_BYTES];
    read_exact(&file, 0, &mut header)?;
    load_elf(
        vfs,
        context,
        file,
        access,
        &header,
        args,
        credentials,
        stack_limit,
    )
}

/// 所有入口共用两映像与限额快照；全部准备结束前不发表 Mm 或改变原用户现场。
fn load_elf(
    vfs: &Vfs,
    fs_context: &FsContext,
    file: FileRef,
    access: ExecAccess,
    header: &[u8; ELF_HEADER_BYTES],
    args: &Arguments,
    credentials: Credentials,
    stack_limit: u64,
) -> Result<LoadedImage, ExecError> {
    let elf = Elf::read(&file, header, MAIN_DYN_BASE)?;
    let interpreter = if let Some(pathname) = &elf.interp {
        let (file, access) = open_executable(vfs, fs_context, pathname, credentials)?;
        let mut header = [0; ELF_HEADER_BYTES];
        read_exact(&file, 0, &mut header)?;
        let image = Elf::read(&file, &header, INTERP_DYN_BASE)?;
        if image.interp.is_some() {
            return Err(ExecError::Format);
        }
        validate_images(&elf, &image)?;
        Some((file, access, image))
    } else {
        None
    };
    let bottom = stack_bottom(
        stack_limit,
        &elf,
        interpreter.as_ref().map(|(_, _, image)| image),
    )?;
    let mut mm = Mm::try_new()?;
    map_segments(&mut mm, &file, &elf.segments)?;
    let (entry, base) = if let Some((file, _, image)) = &interpreter {
        map_segments(&mut mm, file, &image.segments)?;
        (image.entry, image.bias)
    } else {
        (elf.entry, 0)
    };
    map_return_page(&mut mm)?;
    let sp = make_stack(
        &mm,
        &elf,
        base,
        args,
        credentials.uid,
        credentials.gid,
        bottom,
    )?;
    let heap = page_end(elf.end)?;
    mm.set_heap(heap, heap)?;
    let context = UserContext::new(entry, sp);
    mm.bind_executable(file, access);
    if let Some((file, access, _)) = interpreter {
        mm.bind_interpreter(file, access);
    }
    let mm = Arc::try_new(mm).map_err(|_| ExecError::NoMemory)?;
    Ok(LoadedImage { mm, context })
}

/// 只保存装载所需的有界元数据，不创建完整文件的第二数据源。
struct Elf {
    kind: u16,
    max_align: usize,
    interp: Option<Vec<u8>>,
    bias: usize,
    segments: Vec<Segment>,
    entry: usize,
    phdr: usize,
    phnum: usize,
    end: usize,
}

/// LOAD 的字节区间与文件前缀在校验后均不会回绕。
struct Segment {
    start: usize,
    file_end: usize,
    end: usize,
    offset: u64,
    flags: u32,
}

/// 程序头按固定小端字节解码，不借用磁盘字节为任意 Rust 结构体。
struct ProgramHeader {
    kind: u32,
    flags: u32,
    offset: u64,
    address: usize,
    filesz: usize,
    memsz: usize,
    align: usize,
}

impl ProgramHeader {
    /// 读取已知完整的 ELF64 PHDR；目标为 64 位，所有偏移由固定布局给出。
    fn decode(bytes: &[u8]) -> Self {
        Self {
            kind: word32(bytes, 0),
            flags: word32(bytes, 4),
            offset: word64(bytes, 8),
            address: word64(bytes, 16) as usize,
            filesz: word64(bytes, 32) as usize,
            memsz: word64(bytes, 40) as usize,
            align: word64(bytes, 48) as usize,
        }
    }

    /// 任何参与装载的文件跨度及地址跨度都在执行映射前检查。
    fn validate(&self, size: u64) -> Result<(), ExecError> {
        let file_end = self
            .offset
            .checked_add(self.filesz as u64)
            .ok_or(ExecError::Format)?;
        self.address
            .checked_add(self.memsz)
            .ok_or(ExecError::Format)?;
        if size < file_end || (1 < self.align && !self.align.is_power_of_two()) {
            return Err(ExecError::Format);
        }
        Ok(())
    }

    /// 未偏移 LOAD 只校验源跨度和声明同余，用户范围在统一偏移确定后检查。
    fn segment(&self) -> Result<Segment, ExecError> {
        if self.memsz < self.filesz
            || self.flags & !7 != 0
            || self.address & (PAGE_SIZE - 1) != self.offset as usize & (PAGE_SIZE - 1)
            || (1 < self.align
                && self.address & (self.align - 1) != self.offset as usize & (self.align - 1))
        {
            return Err(ExecError::Format);
        }
        let end = self
            .address
            .checked_add(self.memsz)
            .ok_or(ExecError::Format)?;
        #[cfg(target_arch = "riscv64")]
        if self.flags & PF_W != 0 && self.flags & PF_R == 0 {
            return Err(ExecError::Format);
        }
        Ok(Segment {
            start: self.address,
            file_end: self
                .address
                .checked_add(self.filesz)
                .ok_or(ExecError::Format)?,
            end,
            offset: self.offset,
            flags: self.flags,
        })
    }
}

impl Elf {
    /// 首部、程序头和模板复用同一文件，先收集真实对齐再为全部地址施加统一偏移。
    fn read(
        file: &FileRef,
        header: &[u8; ELF_HEADER_BYTES],
        dyn_base: usize,
    ) -> Result<Self, ExecError> {
        let stat = file.stat()?;
        if stat.kind != InodeKind::Regular {
            return Err(ExecError::File(FsError::IsDirectory));
        }
        validate_header(header, stat.size)?;
        let entry = word64(header, 24) as usize;
        let phoff = word64(header, 32);
        let phnum = word16(header, 56) as usize;
        let table_bytes = phnum * PHDR_BYTES;
        let mut table = Vec::new();
        table
            .try_reserve_exact(table_bytes)
            .map_err(|_| ExecError::NoMemory)?;
        table.resize(table_bytes, 0);
        read_exact(file, phoff, &mut table)?;
        let mut segments = Vec::new();
        segments
            .try_reserve_exact(phnum)
            .map_err(|_| ExecError::NoMemory)?;
        let mut declared_phdr = None;
        let mut tls = None;
        let mut interp = None;
        let mut max_align = PAGE_SIZE;
        for bytes in table.chunks_exact(PHDR_BYTES) {
            let ph = ProgramHeader::decode(bytes);
            if ph.kind == 0 {
                continue;
            }
            ph.validate(stat.size)?;
            match ph.kind {
                PT_LOAD => {
                    max_align = max(max_align, ph.align);
                    let segment = ph.segment()?;
                    if segment.start != segment.end {
                        segments.push(segment);
                    }
                }
                PT_INTERP => {
                    if interp.is_some() {
                        return Err(ExecError::Format);
                    }
                    interp = Some(read_interpreter(file, &ph)?);
                }
                // 动态表由用户解释器消费，内核只校验程序头描述的文件与地址跨度。
                PT_DYNAMIC => {}
                5 => return Err(ExecError::Format),
                PT_TLS => {
                    if tls.is_some()
                        || ph.memsz < ph.filesz
                        || (1 < ph.align
                            && ph.address & (ph.align - 1) != ph.offset as usize & (ph.align - 1))
                    {
                        return Err(ExecError::Format);
                    }
                    tls = Some(ph);
                }
                PT_PHDR => {
                    if declared_phdr.is_some()
                        || ph.offset != phoff
                        || ph.filesz != table_bytes
                        || ph.memsz != table_bytes
                        || ph.flags & PF_R == 0
                        || ph.flags & !7 != 0
                        || (1 < ph.align
                            && ph.address & (ph.align - 1) != ph.offset as usize & (ph.align - 1))
                    {
                        return Err(ExecError::Format);
                    }
                    declared_phdr = Some(ph.address);
                }
                // 当前用户栈只有 RW；可执行栈要求不能被忽略。
                0x6474_e551 if ph.flags & PF_X != 0 => return Err(ExecError::Format),
                _ => {}
            }
        }
        segments.sort_unstable_by_key(|segment| segment.start);
        let phdr = find_phdr(&segments, phoff, table_bytes, declared_phdr)?;
        if let Some(tls) = tls {
            // libc 自行分配 TLS 零尾；这里只要求非零模板确由可读 LOAD 的原文件前缀提供。
            if !source_covered(&segments, tls.address, tls.offset, tls.filesz, true) {
                return Err(ExecError::Format);
            }
        }
        let mut elf = Self {
            kind: word16(header, 16),
            max_align,
            interp,
            bias: 0,
            segments,
            entry,
            phdr,
            phnum,
            end: 0,
        };
        elf.relocate(dyn_base)?;
        Ok(elf)
    }

    /// 地址偏移按最大 LOAD 对齐取整，所有加法检查后才校验实际用户页范围与入口。
    fn relocate(&mut self, dyn_base: usize) -> Result<(), ExecError> {
        self.bias = if self.kind == ET_DYN {
            dyn_base
                .checked_add(self.max_align - 1)
                .ok_or(ExecError::Format)?
                & !(self.max_align - 1)
        } else {
            0
        };
        for segment in &mut self.segments {
            segment.start = segment
                .start
                .checked_add(self.bias)
                .ok_or(ExecError::Format)?;
            segment.file_end = segment
                .file_end
                .checked_add(self.bias)
                .ok_or(ExecError::Format)?;
            segment.end = segment
                .end
                .checked_add(self.bias)
                .ok_or(ExecError::Format)?;
            if segment.start < PAGE_SIZE || LOAD_LIMIT < page_end(segment.end)? {
                return Err(ExecError::Format);
            }
        }
        self.entry = self.entry.checked_add(self.bias).ok_or(ExecError::Format)?;
        self.phdr = self.phdr.checked_add(self.bias).ok_or(ExecError::Format)?;
        self.end = validate_segments(&self.segments, self.entry)?;
        Ok(())
    }
}

/// 解释器路径保留原始字节；必须完整终止且不包含截断路径或内嵌 NUL。
fn read_interpreter(file: &FileRef, ph: &ProgramHeader) -> Result<Vec<u8>, ExecError> {
    if ph.filesz < 2 || MAX_PATH_BYTES < ph.filesz {
        return Err(ExecError::Format);
    }
    let mut pathname = Vec::new();
    pathname
        .try_reserve_exact(ph.filesz)
        .map_err(|_| ExecError::NoMemory)?;
    pathname.resize(ph.filesz, 0);
    read_exact(file, ph.offset, &mut pathname)?;
    if pathname.pop() != Some(0) || pathname.contains(&0) {
        return Err(ExecError::Format);
    }
    Ok(pathname)
}

/// 两文件不能占用同一硬件页，即使其有效字节区间在该页内互不相交。
fn validate_images(main: &Elf, interpreter: &Elf) -> Result<(), ExecError> {
    for left in &main.segments {
        for right in &interpreter.segments {
            if page_start(left.start) < page_end(right.end)?
                && page_start(right.start) < page_end(left.end)?
            {
                return Err(ExecError::Format);
            }
        }
    }
    Ok(())
}

/// 校验执行类别、目标、ABI 与全部元数据跨度；扩展 section/PHDR 编号不在支持范围。
fn validate_header(header: &[u8; ELF_HEADER_BYTES], size: u64) -> Result<(), ExecError> {
    #[cfg(target_arch = "riscv64")]
    const MACHINE: u16 = 243;
    #[cfg(target_arch = "loongarch64")]
    const MACHINE: u16 = 258;
    let phnum = word16(header, 56) as usize;
    let phoff = word64(header, 32);
    let phbytes = phnum.checked_mul(PHDR_BYTES).ok_or(ExecError::Format)?;
    let phend = phoff.checked_add(phbytes as u64).ok_or(ExecError::Format)?;
    if header[..7] != *b"\x7fELF\x02\x01\x01"
        || !matches!(header[7], 0 | 3)
        || header[8] != 0
        || !matches!(word16(header, 16), ET_EXEC | ET_DYN)
        || word16(header, 18) != MACHINE
        || word32(header, 20) != 1
        || word16(header, 52) as usize != ELF_HEADER_BYTES
        || word16(header, 54) as usize != PHDR_BYTES
        || phnum == 0
        || PAGE_SIZE < phbytes
        || phoff < ELF_HEADER_BYTES as u64
        || size < phend
        || !supported_flags(word32(header, 48))
    {
        return Err(ExecError::Format);
    }
    let shoff = word64(header, 40);
    let shnum = word16(header, 60) as u64;
    let shstr = word16(header, 62) as u64;
    if shnum == 0 {
        if shoff != 0 || shstr != 0 {
            return Err(ExecError::Format);
        }
    } else {
        let shend = shoff.checked_add(shnum * 64).ok_or(ExecError::Format)?;
        if shoff < ELF_HEADER_BYTES as u64
            || size < shend
            || word16(header, 58) != 64
            || shnum <= shstr
        {
            return Err(ExecError::Format);
        }
    }
    Ok(())
}

/// RV64GC 支持 soft/single/double ABI，不支持 RVE、quad 或需额外平台保证的 TSO。
#[cfg(target_arch = "riscv64")]
fn supported_flags(flags: u32) -> bool {
    flags & !7 == 0 && flags & 6 != 6
}

/// LA64 接受 LP64 soft/single/double 和已定义的对象 ABI v0/v1，拒绝 ILP32。
#[cfg(target_arch = "loongarch64")]
fn supported_flags(flags: u32) -> bool {
    flags & !0x47 == 0 && matches!(flags & 7, 1..=3)
}

/// 对排序后的 LOAD 检查实际页数、跨度、入口权限及所有真实字节重叠。
fn validate_segments(segments: &[Segment], entry: usize) -> Result<usize, ExecError> {
    let first = segments.first().ok_or(ExecError::Format)?;
    let mut end = first.start;
    let mut pages = 0;
    let mut previous_page_end = page_start(first.start);
    let mut executable_entry = false;
    for (index, segment) in segments.iter().enumerate() {
        end = max(end, segment.end);
        let stop = page_end(segment.end)?;
        let start = max(page_start(segment.start), previous_page_end);
        if start < stop {
            pages += (stop - start) / PAGE_SIZE;
        }
        previous_page_end = max(previous_page_end, stop);
        executable_entry |= segment.start <= entry
            && entry < segment.end
            && segment.flags & (PF_R | PF_X) == (PF_R | PF_X);
        for earlier in &segments[..index] {
            validate_overlap(earlier, segment)?;
        }
    }
    #[cfg(target_arch = "riscv64")]
    let entry_aligned = entry & 1 == 0;
    #[cfg(target_arch = "loongarch64")]
    let entry_aligned = entry & 3 == 0;
    if MAX_IMAGE_PAGES < pages
        || MAX_IMAGE_SPAN < end - first.start
        || !executable_entry
        || !entry_aligned
    {
        return Err(ExecError::Format);
    }
    Ok(end)
}

/// 同一字节只能同时指向同一文件位置，或同时属于零尾；页边缘互不重叠可以组合。
fn validate_overlap(left: &Segment, right: &Segment) -> Result<(), ExecError> {
    let start = max(left.start, right.start);
    let end = min(left.end, right.end);
    if end <= start {
        return Ok(());
    }
    let left_file_end = max(start, min(end, left.file_end));
    let right_file_end = max(start, min(end, right.file_end));
    if left_file_end != right_file_end
        || (start < left_file_end
            && left.offset + (start - left.start) as u64
                != right.offset + (start - right.start) as u64)
    {
        return Err(ExecError::Format);
    }
    Ok(())
}

/// PHDR 必须全部在可读文件字节中且保持原文件偏移，允许多个相邻 LOAD 共同覆盖。
fn find_phdr(
    segments: &[Segment],
    offset: u64,
    length: usize,
    declared: Option<usize>,
) -> Result<usize, ExecError> {
    if let Some(address) = declared {
        if source_covered(segments, address, offset, length, true) {
            return Ok(address);
        }
        return Err(ExecError::Format);
    }
    for segment in segments {
        let address = if segment.offset <= offset {
            segment
                .start
                .checked_add((offset - segment.offset) as usize)
        } else {
            segment
                .start
                .checked_sub((segment.offset - offset) as usize)
        };
        if let Some(address) = address {
            if source_covered(segments, address, offset, length, true) {
                return Ok(address);
            }
        }
    }
    Err(ExecError::Format)
}

/// 验证整个区间由同一位置关系的文件字节覆盖，不把页内零填充当作有效 PHDR。
fn source_covered(
    segments: &[Segment],
    address: usize,
    offset: u64,
    length: usize,
    readable: bool,
) -> bool {
    let Some(end) = address.checked_add(length) else {
        return false;
    };
    let Some(_) = offset.checked_add(length as u64) else {
        return false;
    };
    let mut cursor = address;
    while cursor < end {
        let mut stop = cursor;
        for segment in segments {
            if segment.start <= cursor
                && cursor < segment.file_end
                && (!readable || segment.flags & PF_R != 0)
                && segment.offset + (cursor - segment.start) as u64
                    == offset + (cursor - address) as u64
            {
                stop = max(stop, min(end, segment.file_end));
            }
        }
        if stop == cursor {
            return false;
        }
        cursor = stop;
    }
    true
}

/// 沿真实正前缀读到缓冲尾或 EOF，返回实际字节数，未读取部分保持调用者的初始化值。
fn read_prefix(file: &FileRef, offset: u64, bytes: &mut [u8]) -> Result<usize, ExecError> {
    let mut copied = 0;
    while copied < bytes.len() {
        let read = file.read_at_into(
            offset + copied as u64,
            &mut ReadBuffer::kernel(&mut bytes[copied..]),
            FileIoContext::Kernel,
        )?;
        if read == 0 {
            break;
        }
        assert!(
            read <= bytes.len() - copied,
            "file read exceeded requested bytes"
        );
        copied += read;
    }
    Ok(copied)
}

/// 完整读取沿同一前缀算法，EOF 前不足请求长度报告 ShortIo，真实文件错误直接返回。
fn read_exact(file: &FileRef, offset: u64, bytes: &mut [u8]) -> Result<(), ExecError> {
    if read_prefix(file, offset, bytes)? < bytes.len() {
        return Err(ExecError::File(FsError::Io(BlockError::ShortIo)));
    }
    Ok(())
}

/// 每个 VA 页只提交一次；连续完整文件页合为一个 VMA，边缘和零尾使用独占原页。
fn map_segments(mm: &mut Mm, file: &FileRef, segments: &[Segment]) -> Result<(), ExecError> {
    let mut cursor = page_start(segments[0].start);
    for segment in segments {
        cursor = max(cursor, page_start(segment.start));
        let stop = page_end(segment.end)?;
        while cursor < stop {
            let (flags, source) = page_plan(segments, cursor).expect("LOAD page has no source");
            let permissions = permissions(flags);
            if let Some(offset) = source {
                let mut end = cursor + PAGE_SIZE;
                while let Some((next_flags, Some(next_offset))) = page_plan(segments, end) {
                    if next_flags != flags || next_offset != offset + (end - cursor) as u64 {
                        break;
                    }
                    end += PAGE_SIZE;
                }
                mm.map_file(
                    cursor,
                    end - cursor,
                    permissions,
                    RWX,
                    Arc::clone(file),
                    offset,
                    true,
                )?;
                cursor = end;
            } else {
                map_edge(mm, file, segments, cursor, permissions)?;
                cursor += PAGE_SIZE;
            }
        }
    }
    Ok(())
}

/// 合并所有段在一个硬件页上的权限，只在整页同源且无空洞或 BSS 时返回文件页位置。
fn page_plan(segments: &[Segment], base: usize) -> Option<(u32, Option<u64>)> {
    let end = base + PAGE_SIZE;
    let mut flags = 0;
    let mut present = false;
    let mut source = None;
    for segment in segments {
        if segment.start < end && base < segment.end {
            present = true;
            flags |= segment.flags;
        }
        if segment.start <= base && base < segment.file_end {
            source = Some(segment.offset + (base - segment.start) as u64);
        }
    }
    if !present {
        return None;
    }
    source = source.filter(|offset| {
        offset & (PAGE_SIZE as u64 - 1) == 0
            && source_covered(segments, base, *offset, PAGE_SIZE, false)
    });
    Some((flags, source))
}

/// 完整清零未映射的候选页，然后每个有效文件字节只填一次；空洞和所有 BSS 保持零。
fn map_edge(
    mm: &mut Mm,
    file: &FileRef,
    segments: &[Segment],
    base: usize,
    current: PagePermissions,
) -> Result<(), ExecError> {
    let mut pages = alloc_pages(0, true).ok_or(ExecError::NoMemory)?;
    {
        // SAFETY: 新 order0 页由 pages 独占且已完整清零，尚无任何用户、硬件或设备映射；
        // 切片只存在于本次文件填充，全部访问结束后才消费 pages 安装用户映射。
        let bytes =
            unsafe { core::slice::from_raw_parts_mut(pages.as_mut_ptr().cast::<u8>(), PAGE_SIZE) };
        fill_edge(file, segments, base, bytes)?;
    }
    // SAFETY: 候选页全部初始化为零或精确读取的文件字节，填充切片已经结束；
    // mm 尚未共享或发表，pages 为唯一 order0 拥有，之后只由 Mm 管理用户内容。
    unsafe { mm.map_owned(base, pages, current, RWX) }?;
    Ok(())
}

/// 按虚拟字节位置填入文件前缀，预检过的同源重叠不会重复读取或覆盖边缘内容。
fn fill_edge(
    file: &FileRef,
    segments: &[Segment],
    base: usize,
    bytes: &mut [u8],
) -> Result<(), ExecError> {
    let end = base + PAGE_SIZE;
    let mut cursor = base;
    while cursor < end {
        let mut source: Option<&Segment> = None;
        let mut next = end;
        for segment in segments {
            if segment.start <= cursor && cursor < segment.file_end {
                if source.is_none_or(|previous| previous.file_end < segment.file_end) {
                    source = Some(segment);
                }
            } else if cursor < segment.start && segment.start < segment.file_end {
                next = min(next, segment.start);
            }
        }
        if let Some(segment) = source {
            let stop = min(end, segment.file_end);
            let offset = segment.offset + (cursor - segment.start) as u64;
            read_exact(file, offset, &mut bytes[cursor - base..stop - base])?;
            cursor = stop;
        } else {
            cursor = next;
        }
    }
    Ok(())
}

/// 返回页仅承载真实信号 ABI 指令，固定 RX，不宣称为 vDSO。
fn map_return_page(mm: &mut Mm) -> Result<(), ExecError> {
    let code = arch::signal_return_code();
    assert!(
        code.len() <= PAGE_SIZE,
        "signal return code exceeds its page"
    );
    let mut pages = alloc_pages(0, true).ok_or(ExecError::NoMemory)?;
    // SAFETY: pages 独占完整清零且尚未映射的 order0 页，code 永久可读且与候选页不重叠；
    // 写入范围不超过候选页，此原始访问在 map_owned 转交拥有之前结束。
    unsafe {
        core::ptr::copy_nonoverlapping(code.as_ptr(), pages.as_mut_ptr().cast::<u8>(), code.len())
    };
    // SAFETY: 整页仍完整初始化，全部原始访问已结束；mm 未发表，原页唯一拥有消费一次。
    unsafe { mm.map_owned(arch::SIGNAL_RETURN_PAGE, pages, RX, RX) }?;
    Ok(())
}

/// 将本次 soft 向下取整到基本页；整个栈与未映射 guard 必须位于两映像之上、返回页之下。
fn stack_bottom(
    stack_limit: u64,
    main: &Elf,
    interpreter: Option<&Elf>,
) -> Result<usize, ExecError> {
    let bytes = usize::try_from(stack_limit & !(PAGE_SIZE as u64 - 1))
        .map_err(|_| ExecError::Vm(VmError::InvalidRange))?;
    if bytes == 0 {
        return Err(ExecError::ArgumentsTooBig);
    }
    let bottom = arch::USER_STACK_TOP
        .checked_sub(bytes)
        .filter(|bottom| PAGE_SIZE <= *bottom)
        .ok_or(ExecError::Vm(VmError::InvalidRange))?;
    let guard = bottom
        .checked_sub(PAGE_SIZE)
        .ok_or(ExecError::Vm(VmError::InvalidRange))?;
    if arch::USER_ADDRESS_END < arch::USER_STACK_TOP
        || arch::SIGNAL_RETURN_PAGE < arch::USER_STACK_TOP
    {
        return Err(ExecError::Vm(VmError::InvalidRange));
    }
    let image_end = max(main.end, interpreter.map_or(0, |image| image.end));
    if guard < page_end(image_end)? {
        return Err(ExecError::Vm(VmError::Occupied));
    }
    Ok(bottom)
}

/// 在本次真实栈范围中构造主映像 auxv；参数或指针放不下时不发表候选 Mm。
fn make_stack(
    mm: &Mm,
    elf: &Elf,
    interpreter_base: usize,
    args: &Arguments,
    uid: u32,
    gid: u32,
    bottom: usize,
) -> Result<usize, ExecError> {
    mm.map_anonymous(bottom, arch::USER_STACK_TOP - bottom, RW, RWX)?;
    let mut cursor = arch::USER_STACK_TOP;
    let execfn = stack_string(mm, &mut cursor, &args.filename, bottom)?;
    let mut pointers = [0usize; MAX_ARGUMENT_COUNT];
    for (slot, string) in args.argv.iter().chain(&args.envp).enumerate() {
        pointers[slot] = stack_string(mm, &mut cursor, string, bottom)?;
    }
    let random = stack_random(mm, &mut cursor, bottom)?;
    let auxv = [
        (3, elf.phdr),
        (4, PHDR_BYTES),
        (5, elf.phnum),
        (6, PAGE_SIZE),
        (7, interpreter_base),
        (8, 0),
        (9, elf.entry),
        (11, uid as usize),
        (12, uid as usize),
        (13, gid as usize),
        (14, gid as usize),
        (16, hwcap()),
        (17, 100),
        (23, 0),
        (25, random),
        (31, execfn),
        (0, 0),
    ];
    let words = args.argv.len() + args.envp.len() + 3 + auxv.len() * 2;
    let sp = cursor
        .checked_sub(words * core::mem::size_of::<usize>())
        .ok_or(ExecError::ArgumentsTooBig)?
        & !15;
    if sp < bottom {
        return Err(ExecError::ArgumentsTooBig);
    }
    let mut writer = sp;
    stack_word(mm, &mut writer, args.argv.len())?;
    for pointer in &pointers[..args.argv.len()] {
        stack_word(mm, &mut writer, *pointer)?;
    }
    stack_word(mm, &mut writer, 0)?;
    for pointer in &pointers[args.argv.len()..args.argv.len() + args.envp.len()] {
        stack_word(mm, &mut writer, *pointer)?;
    }
    stack_word(mm, &mut writer, 0)?;
    for (kind, value) in auxv {
        stack_word(mm, &mut writer, kind)?;
        stack_word(mm, &mut writer, value)?;
    }
    Ok(sp)
}

/// 在本次私有栈底界内复制原字节及唯一 NUL，失败保持 uaccess 的实际原因。
fn stack_string(
    mm: &Mm,
    cursor: &mut usize,
    bytes: &[u8],
    bottom: usize,
) -> Result<usize, ExecError> {
    let address = cursor
        .checked_sub(bytes.len() + 1)
        .ok_or(ExecError::ArgumentsTooBig)?;
    if address < bottom {
        return Err(ExecError::ArgumentsTooBig);
    }
    copyout_exact(mm, address, bytes)?;
    copyout_exact(mm, address + bytes.len(), &[0])?;
    *cursor = address;
    Ok(address)
}

/// 在本次栈底界内保留完整16字节初始化区；硬件计数与单调序号只提供非加密字节源。
fn stack_random(mm: &Mm, cursor: &mut usize, bottom: usize) -> Result<usize, ExecError> {
    let address = cursor
        .checked_sub(16)
        .filter(|address| bottom <= *address)
        .ok_or(ExecError::ArgumentsTooBig)?;
    let mut bytes = [0; 16];
    random::fill_insecure(&mut bytes);
    copyout_exact(mm, address, &bytes)?;
    *cursor = address;
    Ok(address)
}

/// 初始栈整数以目标小端机器字序列写入，不创建指向已映射用户页的 Rust 引用。
fn stack_word(mm: &Mm, cursor: &mut usize, word: usize) -> Result<(), ExecError> {
    copyout_exact(mm, *cursor, &word.to_le_bytes())?;
    *cursor += core::mem::size_of::<usize>();
    Ok(())
}

/// 映像准备只接受完整用户拷贝，部分成功不会被发表为完整栈。
fn copyout_exact(mm: &Mm, address: usize, bytes: &[u8]) -> Result<(), ExecError> {
    let progress = mm.copyout(address, bytes);
    if let Some(error) = progress.error {
        return Err(ExecError::Vm(error));
    }
    assert_eq!(
        progress.copied,
        bytes.len(),
        "successful exec copy is incomplete"
    );
    Ok(())
}

/// 当前 RV64GC 平台与用户现场共同支持 IMAFDC；不保存的 V 不对用户声明。
#[cfg(target_arch = "riscv64")]
fn hwcap() -> usize {
    (1 << (b'I' - b'A'))
        | (1 << (b'M' - b'A'))
        | 1
        | (1 << (b'F' - b'A'))
        | (1 << (b'D' - b'A'))
        | (1 << (b'C' - b'A'))
}

/// 仅声明实际 CPU 存在且用户入口、复制与信号路径完整保存的浮点和向量扩展。
#[cfg(target_arch = "loongarch64")]
fn hwcap() -> usize {
    let euen = crate::arch::user_euen();
    1 | ((euen & 1) << 3) | ((euen & 2) << 3) | ((euen & 4) << 3)
}

/// 将已经验证的 ELF 访问位转换为现有 Mm 的权限值。
fn permissions(flags: u32) -> PagePermissions {
    PagePermissions {
        read: flags & PF_R != 0,
        write: flags & PF_W != 0,
        execute: flags & PF_X != 0,
    }
}

/// 页下界只用于已校验的非负用户地址。
fn page_start(address: usize) -> usize {
    address & !(PAGE_SIZE - 1)
}

/// checked 页上界避免未可信长度在对齐时回绕。
fn page_end(address: usize) -> Result<usize, ExecError> {
    Ok(address
        .checked_add(PAGE_SIZE - 1)
        .ok_or(ExecError::Format)?
        & !(PAGE_SIZE - 1))
}

/// 解码已知完整头中固定位置的小端 16 位字段。
fn word16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

/// 解码已知完整头中固定位置的小端 32 位字段。
fn word32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

/// 解码已知完整头中固定位置的小端 64 位字段。
fn word64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}
