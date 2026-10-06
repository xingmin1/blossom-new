//! 从真实磁盘 ELF 验证未发表布局、私有写隔离与实际用户退出。

use alloc::vec::Vec;
use core::cmp::min;
use core::convert::{TryFrom, TryInto};
use core::fmt::Debug;

use crate::fs::ktest::user::{UserOwner, PROGRAM};
use crate::fs::FileRef;
use crate::mm::space::Mm;
use crate::mm::{FolioRef, PAGE_SIZE};
use crate::task;
use crate::task::exec::{self, ExecError, LoadedImage};
use ::ktest::{TestContext, TestError, TestResult};

#[derive(Clone, Copy)]
struct Segment {
    offset: u64,
    start: usize,
    filesz: usize,
    memsz: usize,
}

struct Layout {
    entry: usize,
    phdr: usize,
    segments: [Segment; 2],
}

/// 保留底层具体错误及消费地址，框架失败消息只标识当前检查边界。
fn checked<T, E: Debug>(
    context: &TestContext<'_>,
    operation: &'static str,
    address: u64,
    result: Result<T, E>,
) -> Result<T, TestError> {
    result.map_err(|error| {
        context.diagnostic(format_args!(
            "user-layout operation={} address={:#x} error={:?}",
            operation, address, error
        ));
        TestError(operation)
    })
}

/// 单次按位置读取必须交付完整请求，短读保留文件位置与实际长度。
fn read_file(context: &TestContext<'_>, file: &FileRef, offset: u64, dst: &mut [u8]) -> TestResult {
    let count = checked(
        context,
        "ELF positional read failed",
        offset,
        file.read_at(offset, dst),
    )?;
    assert_eq!(
        count,
        dst.len(),
        "ELF short read at file offset {offset:#x}"
    );
    Ok(())
}

/// 经正式 uaccess 读取未 Ready 的 Mm，不创建用户内存的 Rust 切片。
fn read_mm(context: &TestContext<'_>, mm: &Mm, address: usize, dst: &mut [u8]) -> TestResult {
    let progress = mm.copyin(dst, address);
    if progress.error.is_some() || progress.copied != dst.len() {
        context.diagnostic(format_args!(
            "user-layout copyin address={:#x} requested={} copied={} error={:?}",
            address,
            dst.len(),
            progress.copied,
            progress.error
        ));
    }
    assert_eq!(progress.error, None, "ELF copyin failed at {address:#x}");
    assert_eq!(
        progress.copied,
        dst.len(),
        "ELF copyin incomplete at {address:#x}"
    );
    Ok(())
}

/// 解码完整 ELF64 元数据的固定小端字段，调用位置均位于已读完的头表内。
fn elf_word(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

/// 只读取本真实三程序头夹具的元数据，段边界和入口均来自磁盘。
fn layout(context: &TestContext<'_>, file: &FileRef) -> Result<Layout, TestError> {
    let mut header = [0; 64];
    read_file(context, file, 0, &mut header)?;
    assert_eq!(
        &header[..7],
        b"\x7fELF\x02\x01\x01",
        "disk ELF identification"
    );
    assert_eq!(
        u16::from_le_bytes(header[54..56].try_into().unwrap()),
        56,
        "disk ELF PHENT"
    );
    assert_eq!(
        u16::from_le_bytes(header[56..58].try_into().unwrap()),
        3,
        "disk ELF PHNUM"
    );
    let phoff = elf_word(&header, 32);
    let mut table = [0; 168];
    read_file(context, file, phoff, &mut table)?;
    assert_eq!(
        u32::from_le_bytes(table[..4].try_into().unwrap()),
        6,
        "disk PT_PHDR"
    );
    let segments = core::array::from_fn(|index| {
        let bytes = &table[(index + 1) * 56..(index + 2) * 56];
        assert_eq!(
            u32::from_le_bytes(bytes[..4].try_into().unwrap()),
            1,
            "disk PT_LOAD index={index}"
        );
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            if index == 0 { 5 } else { 6 },
            "disk PT_LOAD permissions index={index}"
        );
        Segment {
            offset: elf_word(bytes, 8),
            start: elf_word(bytes, 16) as usize,
            filesz: elf_word(bytes, 32) as usize,
            memsz: elf_word(bytes, 40) as usize,
        }
    });
    let text = segments[0];
    let phdr = text.start
        + usize::try_from(
            phoff
                .checked_sub(text.offset)
                .expect("PHDR precedes text file range"),
        )
        .expect("PHDR offset exceeds target word");
    assert_eq!(
        elf_word(&table, 16) as usize,
        phdr,
        "PT_PHDR address from real LOAD"
    );
    assert_eq!(
        elf_word(&table, 32),
        table.len() as u64,
        "PT_PHDR file bytes"
    );
    assert!(
        phdr + table.len() <= text.start + text.filesz,
        "PHDR is outside text file bytes"
    );
    let data = segments[1];
    assert!(data.filesz < data.memsz, "real writable LOAD has no BSS");
    assert_ne!(
        data.start & (PAGE_SIZE - 1),
        0,
        "real data start lost page edge"
    );
    assert_ne!(
        (data.start + data.filesz) & (PAGE_SIZE - 1),
        0,
        "real data end lost page edge"
    );
    assert!(
        2 * PAGE_SIZE < data.filesz,
        "real file data does not cross pages"
    );
    assert!(
        2 * PAGE_SIZE < data.memsz - data.filesz,
        "real BSS does not cross pages"
    );
    assert_ne!(
        (data.start + data.memsz) & (PAGE_SIZE - 1),
        0,
        "real BSS end lost page edge"
    );
    Ok(Layout {
        entry: elf_word(&header, 24) as usize,
        phdr,
        segments,
    })
}

/// 逐字节消费全部 LOAD 文件前缀和零尾，块读取同样覆盖非页齐及跨页边缘。
fn verify_segments(
    context: &TestContext<'_>,
    file: &FileRef,
    image: &LoadedImage,
    layout: &Layout,
) -> TestResult {
    assert_eq!(
        image.context.pc, layout.entry,
        "loaded entry differs from disk ELF"
    );
    let mut actual = [0; 512];
    let mut expected = [0; 512];
    for segment in &layout.segments {
        let mut cursor = 0;
        while cursor < segment.memsz {
            let count = min(actual.len(), segment.memsz - cursor);
            expected[..count].fill(0);
            let file_count = min(count, segment.filesz.saturating_sub(cursor));
            if 0 < file_count {
                read_file(
                    context,
                    file,
                    segment.offset + cursor as u64,
                    &mut expected[..file_count],
                )?;
            }
            read_mm(
                context,
                &image.mm,
                segment.start + cursor,
                &mut actual[..count],
            )?;
            for index in 0..count {
                assert_eq!(
                    actual[index],
                    expected[index],
                    "LOAD byte mismatch va={:#x} file_offset={:#x} filesz={} memsz={}",
                    segment.start + cursor + index,
                    segment.offset + (cursor + index) as u64,
                    segment.filesz,
                    segment.memsz
                );
            }
            cursor += count;
        }
    }
    Ok(())
}

/// 读取本 64 位用户 ABI 的一个机器字，错误保留原 Mm 访问诊断。
fn stack_word(context: &TestContext<'_>, mm: &Mm, address: usize) -> Result<usize, TestError> {
    checked(
        context,
        "initial stack word read failed",
        address as u64,
        mm.read_usize(address),
    )
}

/// 比较参数的完整字节与唯一结尾 NUL，不保留任何用户页借用。
fn stack_string(context: &TestContext<'_>, mm: &Mm, address: usize, expected: &[u8]) -> TestResult {
    let mut bytes = [0; 32];
    assert!(
        expected.len() < bytes.len(),
        "fixture string exceeds local comparison buffer"
    );
    assert_ne!(address, 0, "initial stack string pointer is NULL");
    read_mm(context, mm, address, &mut bytes[..expected.len() + 1])?;
    assert_eq!(
        &bytes[..expected.len()],
        expected,
        "initial stack string at {address:#x}"
    );
    assert_eq!(
        bytes[expected.len()],
        0,
        "initial stack string lacks NUL at {address:#x}"
    );
    Ok(())
}

/// 按真实消费者栈位置检查参数终止、无重复的必要 auxv 和文件派生地址。
fn verify_stack(context: &TestContext<'_>, image: &LoadedImage, layout: &Layout) -> TestResult {
    #[cfg(target_arch = "riscv64")]
    let sp = image.context.gpr[2];
    #[cfg(target_arch = "loongarch64")]
    let sp = image.context.gpr[3];
    assert_eq!(sp & 15, 0, "initial SP is not 16-byte aligned: {sp:#x}");
    assert_eq!(stack_word(context, &image.mm, sp)?, 2, "initial argc");
    stack_string(
        context,
        &image.mm,
        stack_word(context, &image.mm, sp + 8)?,
        PROGRAM,
    )?;
    stack_string(
        context,
        &image.mm,
        stack_word(context, &image.mm, sp + 16)?,
        b"raw",
    )?;
    assert_eq!(
        stack_word(context, &image.mm, sp + 24)?,
        0,
        "argv terminator"
    );
    stack_string(
        context,
        &image.mm,
        stack_word(context, &image.mm, sp + 32)?,
        b"USER_SMOKE=1",
    )?;
    assert_eq!(
        stack_word(context, &image.mm, sp + 40)?,
        0,
        "envp terminator"
    );
    #[cfg(target_arch = "riscv64")]
    let hwcap = (1 << 8) | (1 << 12) | 1 | (1 << 5) | (1 << 3) | (1 << 2);
    #[cfg(target_arch = "loongarch64")]
    let hwcap = 1 | (1 << 3);
    let required = [
        (3, layout.phdr),
        (4, 56),
        (5, 3),
        (6, PAGE_SIZE),
        (7, 0),
        (8, 0),
        (9, layout.entry),
        (11, 0),
        (12, 0),
        (13, 0),
        (14, 0),
        (16, hwcap),
        (17, 100),
        (23, 0),
    ];
    let mut found = [false; 14];
    let mut execfn_found = false;
    for slot in 0..32 {
        let address = sp + 48 + slot * 16;
        let kind = stack_word(context, &image.mm, address)?;
        let value = stack_word(context, &image.mm, address + 8)?;
        if kind == 0 {
            assert_eq!(value, 0, "AT_NULL value");
            for (index, (kind, _)) in required.iter().enumerate() {
                assert!(found[index], "missing required auxv kind={}", kind);
            }
            assert!(execfn_found, "missing AT_EXECFN");
            return Ok(());
        }
        assert!(
            !matches!(kind, 25 | 33),
            "auxv declares unavailable random/vDSO kind={}",
            kind
        );
        if kind == 31 {
            assert!(!execfn_found, "duplicate AT_EXECFN");
            execfn_found = true;
            stack_string(context, &image.mm, value, PROGRAM)?;
        }
        if let Some(index) = required.iter().position(|(expected, _)| *expected == kind) {
            assert!(!found[index], "duplicate required auxv kind={}", kind);
            found[index] = true;
            assert_eq!(value, required[index].1, "auxv kind={kind}");
        }
    }
    Err(TestError("initial auxv lacks bounded AT_NULL terminator"))
}

/// 保活数据首尾及中间的真实缓存页，并按位置保存该文件余下字节供私有写后比较。
fn snapshot(
    context: &TestContext<'_>,
    file: &FileRef,
    data: Segment,
) -> Result<(u64, Vec<u8>, Vec<FolioRef>), TestError> {
    let start = data.offset & !(PAGE_SIZE as u64 - 1);
    let size = checked(context, "ELF stat failed", 0, file.stat())?.size;
    let count = usize::try_from(
        size.checked_sub(start)
            .expect("data page starts beyond ELF EOF"),
    )
    .map_err(|_| TestError("ELF cache comparison range exceeds target word"))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(count)
        .map_err(|_| TestError("NoMemory: ELF cache byte snapshot"))?;
    bytes.resize(count, 0);
    read_file(context, file, start, &mut bytes)?;
    let pages = count.div_ceil(PAGE_SIZE);
    let mut folios = Vec::new();
    folios
        .try_reserve_exact(pages)
        .map_err(|_| TestError("NoMemory: ELF cache folio owners"))?;
    for index in 0..pages {
        let page = start / PAGE_SIZE as u64 + index as u64;
        folios.push(checked(
            context,
            "ELF cache folio read failed",
            page * PAGE_SIZE as u64,
            file.file_page(page),
        )?);
    }
    Ok((start, bytes, folios))
}

/// 对未发表映像的全部 data/BSS 私有写并读回，再逐字节检查仍保活的文件缓存。
fn verify_private_write(
    context: &TestContext<'_>,
    file: &FileRef,
    image: &LoadedImage,
    data: Segment,
) -> TestResult {
    let (file_start, before, folios) = snapshot(context, file, data)?;
    let pattern = [0xa5; 512];
    let mut observed = [0; 512];
    let mut cursor = 0;
    while cursor < data.memsz {
        let count = min(pattern.len(), data.memsz - cursor);
        let address = data.start + cursor;
        let progress = image.mm.copyout(address, &pattern[..count]);
        if progress.error.is_some() || progress.copied != count {
            context.diagnostic(format_args!(
                "user-layout private-write address={:#x} requested={} copied={} error={:?}",
                address, count, progress.copied, progress.error
            ));
        }
        assert_eq!(
            progress.error, None,
            "private data/BSS copyout at {address:#x}"
        );
        assert_eq!(
            progress.copied, count,
            "private data/BSS copyout prefix at {address:#x}"
        );
        read_mm(context, &image.mm, address, &mut observed[..count])?;
        for (index, byte) in observed[..count].iter().enumerate() {
            assert_eq!(
                *byte,
                0xa5,
                "private data/BSS write readback va={:#x}",
                address + index
            );
        }
        cursor += count;
    }
    for (block, expected) in before.chunks(observed.len()).enumerate() {
        let offset = file_start + (block * observed.len()) as u64;
        read_file(context, file, offset, &mut observed[..expected.len()])?;
        for (index, byte) in expected.iter().enumerate() {
            assert_eq!(
                observed[index],
                *byte,
                "private write polluted file cache offset={:#x}",
                offset + index as u64
            );
        }
    }
    drop(folios);
    Ok(())
}

/// 从真实盘正式装载，检查私有布局后丢弃修改映像，再运行重新装载的普通用户程序。
pub(super) fn run(context: &mut TestContext<'_>, fixture: &mut UserOwner) -> TestResult {
    fixture.prepare(context)?;
    let file = fixture.file(context, PROGRAM)?;
    let image = fixture.load(context)?;
    let layout = layout(context, &file)?;
    verify_segments(context, &file, &image, &layout)?;
    verify_stack(context, &image, &layout)?;
    verify_private_write(context, &file, &image, layout.segments[1])?;
    drop(image);

    let interpreter = fixture.file(context, b"/test/bin/user-interp")?;
    let arguments = checked(
        context,
        "ELF arguments preparation failed",
        0,
        fixture.arguments(),
    )?;
    match exec::load(interpreter, &arguments, 0, 0) {
        Err(ExecError::Format) => {}
        Err(error) => {
            context.diagnostic(format_args!(
                "user-layout PT_INTERP unexpected error={:?}",
                error
            ));
            return Err(TestError("PT_INTERP did not return Format"));
        }
        Ok(image) => {
            drop(image);
            return Err(TestError("unsupported PT_INTERP was loaded"));
        }
    }
    drop(arguments);
    drop(file);

    let pristine = fixture.load(context)?;
    let coordinator = task::current_task().id();
    fixture.start(context, pristine)?;
    let event = fixture.wait(context)?;
    context.diagnostic(format_args!(
        "user-layout actual-exit pid={} status={} user_interrupts={} task_id={:?} coordinator={:?}",
        event.pid, event.status, event.user_interrupts, event.task_id, coordinator
    ));
    assert_eq!(
        event.status, 0,
        "ordinary ELF user program did not exit normally with status 0"
    );
    assert!(
        0 < event.user_interrupts,
        "ordinary ELF never observed a real user interrupt"
    );
    assert!(
        0 < event.user_timer_interrupts,
        "ordinary ELF never observed a real user timer interrupt"
    );
    assert_ne!(
        event.task_id, coordinator,
        "user ELF ran as the kernel coordinator Task"
    );
    Ok(())
}
