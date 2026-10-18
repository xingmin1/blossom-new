//! 磁盘主映像与真实 ELF 解释器在同一私有 Mm 中准备，文件整页同源，边缘页先初始化。

mod elf;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::convert::{TryFrom, TryInto};

use self::elf::{page_end, Elf, ELF_HEADER_BYTES, INTERP_DYN_BASE, MAIN_DYN_BASE};

use crate::arch::{self, UserContext};
use crate::block::BlockError;
use crate::fs::{
    self, ExecAccess, FileIoContext, FileRef, FsContext, FsError, ReadBuffer, ResolveMode, Vfs,
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
const SCRIPT_HEADER_BYTES: usize = 256;
const MAX_INTERPRETER_DEPTH: usize = 5;
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
    let (mut file, mut access) = open_executable(vfs, context, args.filename())?;
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
        let next = open_executable(vfs, context, &args.argv[0])?;
        // 新文件已经取得写排斥，接替后才放掉上一个脚本的文件与执行写占用。
        (file, access) = next;
    }
    Err(ExecError::TooManyInterpreters)
}

/// 沿当前 root/cwd 取得真实可映射文件，首读前保留原 inode 的执行写占用。
fn open_executable(
    vfs: &Vfs,
    context: &FsContext,
    pathname: &[u8],
) -> Result<(FileRef, ExecAccess), ExecError> {
    let path = vfs.resolve_from(context, context.cwd(), pathname, ResolveMode::Follow, None)?;
    let file = fs::open_path(vfs, path)?;
    let access = file
        .mapping_inode()
        .ok_or(ExecError::File(FsError::Permission))?
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
    let mut elf = Elf::read(&file, header, MAIN_DYN_BASE)?;
    let interpreter = if let Some(pathname) = &elf.interp {
        let (file, access) = open_executable(vfs, fs_context, pathname)?;
        let mut header = [0; ELF_HEADER_BYTES];
        read_exact(&file, 0, &mut header)?;
        let image = Elf::read(&file, &header, INTERP_DYN_BASE)?;
        if image.interp.is_some() {
            return Err(ExecError::Format);
        }
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
    elf::map_images(
        &mut mm,
        &file,
        &mut elf,
        interpreter.as_ref().map(|(file, _, image)| (file, image)),
    )?;
    let (entry, base) = interpreter
        .as_ref()
        .map_or((elf.entry, 0), |(_, _, image)| (image.entry, image.bias));
    let stack_permissions = if elf.executable_stack
        || interpreter
            .as_ref()
            .is_some_and(|(_, _, image)| image.executable_stack)
    {
        RWX
    } else {
        RW
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
        stack_permissions,
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

/// 将本次 soft 向下取整到基本页，只检查真实栈及 guard 与映像页的冲突。
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
    if main.overlaps(guard, arch::USER_STACK_TOP)
        || interpreter.is_some_and(|image| image.overlaps(guard, arch::USER_STACK_TOP))
    {
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
    stack_permissions: PagePermissions,
) -> Result<usize, ExecError> {
    mm.map_anonymous(
        bottom,
        arch::USER_STACK_TOP - bottom,
        stack_permissions,
        RWX,
    )?;
    let mut cursor = arch::USER_STACK_TOP;
    let execfn = stack_string(mm, &mut cursor, &args.filename, bottom)?;
    let mut pointers = [0usize; MAX_ARGUMENT_COUNT];
    for (slot, string) in args.argv.iter().chain(&args.envp).enumerate() {
        pointers[slot] = stack_string(mm, &mut cursor, string, bottom)?;
    }
    let random = stack_random(mm, &mut cursor, bottom)?;
    let auxv = [
        (3, elf.phdr.unwrap_or(0)),
        (4, elf.phent),
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
