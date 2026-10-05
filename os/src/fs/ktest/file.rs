//! 具体打开文件、固定描述符表和真实异步拥有的消费回归。

use crate::fs::ktest::path::PathOwner;
use crate::fs::ktest::*;
use crate::fs::{DescriptorFlags, FileTable, OpenOptions, SeekFrom};

struct FileOwner {
    path: PathOwner,
    table: Option<Arc<FileTable>>,
    hook_volume: Option<Arc<Ext4Volume>>,
}

impl FileOwner {
    /// 正文构造完整路径视图与固定表，失败时保留原设备的清理责任。
    fn prepare(&mut self, context: &TestContext<'_>, capacity: usize) -> TestResult {
        self.path.prepare_path(context)?;
        let table = Arc::try_new(checked(context, FileTable::try_new(capacity))?)
            .map_err(|_| TestError("file table owner OOM"))?;
        self.table = Some(table);
        Ok(())
    }

    /// 正文借用完整表，静止化以后不再允许发表描述符。
    fn table(&self) -> &Arc<FileTable> {
        self.table
            .as_ref()
            .expect("file table consumed before body")
    }

    /// 使用本例真实根和工作目录打开字节路径。
    fn open(
        &self,
        context: &TestContext<'_>,
        pathname: &[u8],
        options: OpenOptions,
    ) -> Result<usize, TestError> {
        checked(
            context,
            self.table()
                .open(self.path.vfs(), self.path.context(), pathname, options),
        )
    }

    /// 本例首次读取的冷页在首段收权后暂停，回调不持表锁或卷锁。
    fn hold_first_page(&mut self, context: &TestContext<'_>) -> TestResult {
        let path = checked(
            context,
            self.path
                .vfs()
                .resolve(self.path.context(), b"/pattern", ResolveMode::Follow),
        )?;
        let volume = path.filesystem().clone();
        let observer = self.path.owner.observer().clone();
        let block_size = volume.geometry().block_size;
        let hook: Arc<dyn Fn(u64, usize) + Send + Sync> = Arc::try_new(move |page, bytes| {
            if page == 1 && bytes == block_size && observer.hold_loader.load(Ordering::Acquire) {
                observer.loader_entered.fetch_add(1, Ordering::AcqRel);
                observer.gate.wait();
            }
        })
        .map_err(|_| TestError("file page observer OOM"))?;
        self.path.vfs().dcache().ktest_set_lookup_hook(None);
        volume.ktest_set_segment_hook(Some(hook));
        self.hook_volume = Some(volume);
        self.path
            .owner
            .observer()
            .hold_loader
            .store(true, Ordering::Release);
        Ok(())
    }
}

impl Fixture<DeviceInventory> for FileOwner {
    /// 准备阶段只拥有完整旧夹具，路径、表和观察入口均由正文准备。
    fn set_up(
        context: &mut TestContext<'_>,
        inventory: &DeviceInventory,
    ) -> Result<Self, TestError> {
        Ok(Self {
            path: <PathOwner as Fixture<DeviceInventory>>::set_up(context, inventory)?,
            table: None,
            hook_volume: None,
        })
    }

    /// 先撤自己的页观察并放行，再释放表；旧路径夹具唯一负责真实 join 和设备停止。
    fn quiesce(&mut self, context: &mut TestContext<'_>) -> TestResult {
        if let Some(volume) = &self.hook_volume {
            volume.ktest_set_segment_hook(None);
        }
        self.path
            .owner
            .observer()
            .hold_loader
            .store(false, Ordering::Release);
        self.path.owner.observer().gate.complete();
        drop(self.table.take());
        drop(self.hook_volume.take());
        <PathOwner as Fixture<DeviceInventory>>::quiesce(&mut self.path, context)
    }

    /// 原路径夹具消费设备拥有者并核第二层完整六域。
    fn tear_down(self, context: &mut TestContext<'_>) -> TestResult {
        let Self {
            path,
            table,
            hook_volume,
        } = self;
        drop(table);
        drop(hook_volume);
        <PathOwner as Fixture<DeviceInventory>>::tear_down(path, context)
    }
}

/// 每个字节均与外部文件图样比较，并核消费者实际交付的长度。
fn read_pattern(
    context: &mut TestContext<'_>,
    table: &FileTable,
    fd: usize,
    offset: u64,
    len: usize,
) -> TestResult {
    let mut bytes = [0xa7; mm::PAGE_SIZE];
    context.expect_eq(checked(context, table.read(fd, &mut bytes[..len]))?, len);
    context.expect_eq(patterned(&bytes[..len], offset), true);
    Ok(())
}

/// 静态宏锚点交给既有父入口，不维护第二份用例列表。
pub(crate) fn anchors() -> [&'static ::ktest::SuiteSlot; 7] {
    [
        regular::__ktest_anchor(),
        directories::__ktest_anchor(),
        slots::__ktest_anchor(),
        racing::__ktest_anchor(),
        prefix::__ktest_anchor(),
        mounts::__ktest_anchor(),
        oom::__ktest_anchor(),
    ]
}

#[ktest::suite(fixture = FileOwner, suite_resources = DeviceInventory, context = "Thread", tags("fs-file", "fs-file-regular"), resources("root"), timeout_ms = 90000)]
pub(crate) mod regular {
    use super::*;

    /// 独立打开和硬链接各有位置，复制描述符共享位置，定位读不修改位置且元数据来自真实 inode。
    #[test]
    #[ktest(tags("B20", "B21", "B22"))]
    fn independent_shared_cursor_and_bytes(
        context: &mut TestContext<'_>,
        fixture: &mut FileOwner,
    ) -> TestResult {
        fixture.prepare(context, 8)?;
        let fd = fixture.open(
            context,
            b"/pattern",
            OpenOptions {
                close_on_exec: true,
                ..OpenOptions::default()
            },
        )?;
        let independent = fixture.open(context, b"/pattern", OpenOptions::default())?;
        let hard = fixture.open(context, b"/hardlink", OpenOptions::default())?;
        let table = fixture.table();
        let first = checked(context, table.lookup(fd))?;
        let second = checked(context, table.lookup(independent))?;
        let hard_file = checked(context, table.lookup(hard))?;
        context.expect_eq(Arc::ptr_eq(&first, &second), false);
        context.expect_eq(Arc::ptr_eq(&first, &hard_file), false);
        let stat = checked(context, table.stat(fd))?;
        context.expect_eq(checked(context, table.stat(hard))?, stat);
        let path = checked(
            context,
            fixture
                .path
                .vfs()
                .resolve(fixture.path.context(), b"/pattern", ResolveMode::Follow),
        )?;
        let inode = checked(context, path.inode())?;
        let data = checked(context, inode.metadata())?;
        context.expect_eq(
            (
                stat.inode,
                stat.kind,
                stat.mode,
                stat.size,
                stat.uid,
                stat.gid,
                stat.links,
                stat.blocks,
            ),
            (
                inode.number(),
                data.kind,
                data.mode,
                data.size,
                data.uid,
                data.gid,
                data.links,
                data.blocks,
            ),
        );
        context.expect_eq(stat.size, PATTERN_SIZE);
        read_pattern(context, table, fd, 0, 17)?;
        read_pattern(context, table, independent, 0, 11)?;
        read_pattern(context, table, hard, 0, 7)?;
        let duplicate = checked(context, table.dup(fd))?;
        context.expect_eq(
            Arc::ptr_eq(&first, &checked(context, table.lookup(duplicate))?),
            true,
        );
        context.expect_eq(checked(context, table.flags(fd))?.close_on_exec, true);
        context.expect_eq(
            checked(context, table.flags(duplicate))?.close_on_exec,
            false,
        );
        read_pattern(context, table, duplicate, 17, 19)?;
        context.expect_eq(checked(context, table.seek(fd, SeekFrom::Current(0)))?, 36);
        let mut bytes = [0xa7; 29];
        context.expect_eq(
            checked(context, table.read_at(fd, 4091, &mut bytes))?,
            bytes.len(),
        );
        context.expect_eq(patterned(&bytes, 4091), true);
        context.expect_eq(
            checked(context, table.seek(duplicate, SeekFrom::Current(0)))?,
            36,
        );
        context.expect_eq(
            checked(context, table.seek(fd, SeekFrom::Current(-16)))?,
            20,
        );
        context.expect_eq(
            checked(context, table.seek(fd, SeekFrom::End(-5)))?,
            PATTERN_SIZE - 5,
        );
        read_pattern(context, table, duplicate, PATTERN_SIZE - 5, 5)?;
        context.expect_eq(checked(context, table.read(fd, &mut bytes))?, 0);
        context.expect_eq(checked(context, table.read(fd, &mut []))?, 0);
        context.expect_eq(
            table.seek(fd, SeekFrom::Current(i64::MIN)).err(),
            Some(FsError::InvalidOperation),
        );
        context.expect_eq(
            checked(context, table.seek(fd, SeekFrom::Current(0)))?,
            PATTERN_SIZE,
        );
        context.expect_eq(
            checked(context, table.seek(fd, SeekFrom::Start(u64::MAX)))?,
            u64::MAX,
        );
        context.expect_eq(
            table.seek(fd, SeekFrom::Current(1)).err(),
            Some(FsError::InvalidOperation),
        );
        context.expect_eq(
            checked(context, table.seek(fd, SeekFrom::Current(0)))?,
            u64::MAX,
        );
        context.expect_eq(checked(context, table.read(fd, &mut bytes))?, 0);
        context.expect_eq(table.write(fd, b"write").err(), Some(FsError::ReadOnly));
        context.expect_eq(table.write(fd, b"").err(), Some(FsError::ReadOnly));
        context.expect_eq(table.readdir(fd).err(), Some(FsError::NotDirectory));
        checked(context, table.seek(fd, SeekFrom::Start(0)))?;
        let mut offset = 0;
        while offset < PATTERN_SIZE {
            let len = (PATTERN_SIZE - offset).min(mm::PAGE_SIZE as u64) as usize;
            read_pattern(context, table, fd, offset, len)?;
            offset += len as u64;
        }
        context.expect_eq(
            checked(context, table.seek(independent, SeekFrom::Current(0)))?,
            11,
        );
        context.expect_eq(checked(context, table.seek(hard, SeekFrom::Current(0)))?, 7);
        Ok(())
    }
}

/// 逐项核原始目录名字和唯一编号，不缓存或重新实现目录扫描。
fn enumerate_names(
    context: &mut TestContext<'_>,
    table: &FileTable,
    fd: usize,
    indexed: bool,
) -> TestResult {
    let count = if indexed { DIRECTORY_ENTRIES } else { 40 };
    let mut seen = [false; DIRECTORY_ENTRIES];
    let mut dots = 0;
    let mut offset = 0;
    while let Some(entry) = checked(context, table.readdir(fd))? {
        if entry.next_offset <= offset {
            return Err(TestError("file directory cursor failed to advance"));
        }
        offset = entry.next_offset;
        context.expect_eq(
            checked(context, table.seek(fd, SeekFrom::Current(0)))?,
            offset,
        );
        if entry.name == b"." || entry.name == b".." {
            dots += 1;
            continue;
        }
        let digits = if indexed { 5 } else { 4 };
        let len = if indexed { 191 } else { 9 };
        if entry.name.len() != len || &entry.name[..5] != b"item-" {
            return Err(TestError("file directory unexpected byte name"));
        }
        if indexed && (entry.name[10] != b'-' || !entry.name[11..].iter().all(|byte| *byte == b'n'))
        {
            return Err(TestError("file indexed directory name tail changed"));
        }
        let mut index = 0;
        for digit in &entry.name[5..5 + digits] {
            if !digit.is_ascii_digit() {
                return Err(TestError("file directory name digit invalid"));
            }
            index = index * 10 + usize::from(*digit - b'0');
        }
        if count <= index || seen[index] {
            return Err(TestError("file directory duplicate or out of range name"));
        }
        seen[index] = true;
        context.expect_eq(entry.kind, Some(InodeKind::Regular));
    }
    context.expect_eq(dots, 2);
    context.expect_eq(seen[..count].iter().all(|seen| *seen), true);
    context.expect_eq(checked(context, table.readdir(fd))?, None);
    Ok(())
}

#[ktest::suite(fixture = FileOwner, suite_resources = DeviceInventory, context = "Thread", tags("fs-file", "fs-file-directory"), resources("root"), timeout_ms = 90000)]
pub(crate) mod directories {
    use super::*;

    /// 普通与索引目录逐项读出原字节，复制共享 cookie，独立打开与真实类型检查互不混淆。
    #[test]
    #[ktest(tags("B20", "B21", "B22"))]
    fn entries_cookie_types_and_nofollow(
        context: &mut TestContext<'_>,
        fixture: &mut FileOwner,
    ) -> TestResult {
        fixture.prepare(context, 8)?;
        for (pathname, indexed) in [
            (b"/plain".as_slice(), false),
            (b"/indexed".as_slice(), true),
        ] {
            let fd = fixture.open(
                context,
                pathname,
                OpenOptions {
                    directory: true,
                    ..OpenOptions::default()
                },
            )?;
            let independent = fixture.open(context, pathname, OpenOptions::default())?;
            let table = fixture.table();
            let duplicate = checked(context, table.dup(fd))?;
            let first = checked(context, table.readdir(fd))?
                .ok_or(TestError("directory first entry absent"))?;
            let cookie = first.next_offset;
            context.expect_eq(checked(context, table.readdir(independent))?, Some(first));
            let next = checked(context, table.readdir(duplicate))?
                .ok_or(TestError("directory second entry absent"))?;
            checked(context, table.seek(independent, SeekFrom::Start(cookie)))?;
            context.expect_eq(checked(context, table.readdir(independent))?, Some(next));
            context.expect_eq(
                checked(context, table.seek(fd, SeekFrom::Current(0)))?,
                checked(context, table.seek(duplicate, SeekFrom::Current(0)))?,
            );
            checked(context, table.seek(fd, SeekFrom::Start(1)))?;
            context.expect_eq(table.readdir(fd).err(), Some(FsError::InvalidOperation));
            context.expect_eq(checked(context, table.seek(fd, SeekFrom::Current(0)))?, 1);
            context.expect_eq(checked(context, table.seek(fd, SeekFrom::Current(-1)))?, 0);
            let size = checked(context, table.stat(fd))?.size;
            context.expect_eq(checked(context, table.seek(fd, SeekFrom::End(0)))?, size);
            context.expect_eq(checked(context, table.readdir(fd))?, None);
            context.expect_eq(
                table.seek(fd, SeekFrom::End(i64::MIN)).err(),
                Some(FsError::InvalidOperation),
            );
            context.expect_eq(checked(context, table.seek(fd, SeekFrom::Start(0)))?, 0);
            enumerate_names(context, table, duplicate, indexed)?;
            let mut byte = [0xa7; 1];
            context.expect_eq(table.read(fd, &mut byte).err(), Some(FsError::IsDirectory));
            context.expect_eq(
                table.read_at(fd, 0, &mut byte).err(),
                Some(FsError::IsDirectory),
            );
            context.expect_eq(table.write(fd, b"x").err(), Some(FsError::ReadOnly));
            for fd in [fd, independent, duplicate] {
                checked(context, table.close(fd))?;
            }
        }
        let fd = fixture.open(context, b"/", OpenOptions::default())?;
        let table = fixture.table();
        let mut raw = 0;
        while let Some(entry) = checked(context, table.readdir(fd))? {
            if entry.name == b"name-\xff\xfe" {
                context.expect_eq(entry.kind, Some(InodeKind::Regular));
                raw += 1;
            }
        }
        context.expect_eq(raw, 1);
        let followed = fixture.open(context, b"/fastlink", OpenOptions::default())?;
        read_pattern(context, table, followed, 0, 31)?;
        context.expect_eq(
            table
                .open(
                    fixture.path.vfs(),
                    fixture.path.context(),
                    b"/fastlink",
                    OpenOptions {
                        no_follow: true,
                        ..OpenOptions::default()
                    },
                )
                .err(),
            Some(FsError::TooManySymlinks),
        );
        context.expect_eq(
            table
                .open(
                    fixture.path.vfs(),
                    fixture.path.context(),
                    b"/pattern",
                    OpenOptions {
                        directory: true,
                        ..OpenOptions::default()
                    },
                )
                .err(),
            Some(FsError::NotDirectory),
        );
        context.expect_eq(
            table
                .open(
                    fixture.path.vfs(),
                    fixture.path.context(),
                    b"/fastlink",
                    OpenOptions {
                        directory: true,
                        no_follow: true,
                        ..OpenOptions::default()
                    },
                )
                .err(),
            Some(FsError::NotDirectory),
        );
        let raw_fd = fixture.open(context, b"/name-\xff\xfe", OpenOptions::default())?;
        let mut bytes = [0xa7; 9];
        context.expect_eq(checked(context, table.read(raw_fd, &mut bytes))?, 9);
        context.expect_eq(bytes, *b"raw-name\n");
        Ok(())
    }
}

#[ktest::suite(fixture = FileOwner, suite_resources = DeviceInventory, context = "Thread", tags("fs-file", "fs-file-slots"), resources("root"), timeout_ms = 30000)]
pub(crate) mod slots {
    use super::*;

    /// 移动预留只撤自己的槽，精确复制拒绝预留目标，容量与大编号都由真实调用消费。
    #[test]
    #[ktest(tags("B20", "B21"))]
    fn reservation_capacity_reuse_and_wide_fd(
        context: &mut TestContext<'_>,
        fixture: &mut FileOwner,
    ) -> TestResult {
        context.expect_eq(FileTable::try_new(0).err(), Some(FsError::InvalidOperation));
        fixture.prepare(context, 2)?;
        let fd = fixture.open(
            context,
            b"/pattern",
            OpenOptions {
                close_on_exec: true,
                ..OpenOptions::default()
            },
        )?;
        let table = fixture.table();
        context.expect_eq(table.capacity(), 2);
        let reservation = checked(context, table.reserve())?;
        context.expect_eq(reservation.fd(), 1);
        context.expect_eq(table.lookup(1).err(), Some(FsError::BadFd));
        context.expect_eq(table.flags(1).err(), Some(FsError::BadFd));
        context.expect_eq(table.close(1).err(), Some(FsError::BadFd));
        context.expect_eq(table.dup_exact(fd, 1).err(), Some(FsError::Busy));
        context.expect_eq(table.reserve().err(), Some(FsError::TooManyFiles));
        context.expect_eq(table.dup(fd).err(), Some(FsError::TooManyFiles));
        context.expect_eq(
            table
                .open(
                    fixture.path.vfs(),
                    fixture.path.context(),
                    b"/empty",
                    OpenOptions::default(),
                )
                .err(),
            Some(FsError::TooManyFiles),
        );
        drop(reservation);
        let reservation = checked(context, table.reserve())?;
        context.expect_eq(reservation.fd(), 1);
        let file = checked(context, table.lookup(fd))?;
        let second = reservation.publish(file.clone(), DescriptorFlags::default());
        read_pattern(context, table, second, 0, 23)?;
        context.expect_eq(checked(context, table.seek(fd, SeekFrom::Current(0)))?, 23);
        context.expect_eq(checked(context, table.dup_exact(fd, fd))?, fd);
        context.expect_eq(checked(context, table.flags(fd))?.close_on_exec, true);
        context.expect_eq(table.lookup(usize::MAX).err(), Some(FsError::BadFd));
        context.expect_eq(table.dup_exact(fd, 2).err(), Some(FsError::BadFd));
        checked(context, table.close(second))?;
        context.expect_eq(table.close(second).err(), Some(FsError::BadFd));
        context.expect_eq(checked(context, table.dup(fd))?, second);
        checked(context, table.close(fd))?;
        context.expect_eq(table.read(fd, &mut [0; 1]).err(), Some(FsError::BadFd));
        context.expect_eq(
            table
                .open(
                    fixture.path.vfs(),
                    fixture.path.context(),
                    b"/missing",
                    OpenOptions::default(),
                )
                .err(),
            Some(FsError::NotFound),
        );
        context.expect_eq(
            fixture.open(context, b"/empty", OpenOptions::default())?,
            fd,
        );
        context.expect_eq(checked(context, table.dup_exact(second, fd))?, fd);
        context.expect_eq(
            Arc::ptr_eq(&file, &checked(context, table.lookup(fd))?),
            true,
        );
        context.expect_eq(checked(context, table.flags(fd))?.close_on_exec, false);
        let wide = checked(context, FileTable::try_new(300))?;
        let base = checked(context, wide.reserve())?.publish(file, DescriptorFlags::default());
        context.expect_eq(checked(context, wide.dup_exact(base, 299))?, 299);
        read_pattern(context, &wide, 299, 23, 13)?;
        context.expect_eq(checked(context, wide.seek(base, SeekFrom::Current(0)))?, 36);
        checked(context, wide.close(299))?;
        context.expect_eq(wide.lookup(299).err(), Some(FsError::BadFd));
        Ok(())
    }
}

#[ktest::suite(fixture = FileOwner, suite_resources = DeviceInventory, context = "Thread", min_cpus = 2, tags("fs-file", "fs-file-race"), resources("root"), timeout_ms = 90000)]
pub(crate) mod racing {
    use super::*;

    /// 页读暂停时关闭与复用同编号仍保活旧 OFD，另一个真实文件可前进且预留目标保持 Busy。
    #[test]
    #[ktest(tags("B20", "B21", "B22", "B23"))]
    fn read_close_reuse_progress_and_reserved_dup(
        context: &mut TestContext<'_>,
        fixture: &mut FileOwner,
    ) -> TestResult {
        fixture.prepare(context, 4)?;
        let fd = fixture.open(context, b"/pattern", OpenOptions::default())?;
        let table = fixture.table().clone();
        checked(
            context,
            table.seek(fd, SeekFrom::Start(mm::PAGE_SIZE as u64)),
        )?;
        fixture.hold_first_page(context)?;
        fixture.path.owner.publish(
            0,
            Job::FileRead {
                table: table.clone(),
                fd,
                len: mm::PAGE_SIZE,
                expected_offset: mm::PAGE_SIZE as u64,
            },
        );
        until("file read did not reach first segment gate", || {
            fixture
                .path
                .owner
                .observer()
                .loader_entered
                .load(Ordering::Acquire)
                == 1
                && fixture.path.owner.observer().blocked(0)
        })?;
        checked(context, table.close(fd))?;
        let reused = fixture.open(context, b"/name-\xff\xfe", OpenOptions::default())?;
        context.expect_eq(reused, fd);
        let mut bytes = [0xa7; 9];
        context.expect_eq(checked(context, table.read(reused, &mut bytes))?, 9);
        context.expect_eq(bytes, *b"raw-name\n");
        let other = fixture.open(context, b"/deep", OpenOptions::default())?;
        let submitted = fixture.path.owner.disk()?.test_snapshot().submitted;
        read_pattern(context, &table, other, 0, 31)?;
        context.expect_eq(
            submitted < fixture.path.owner.disk()?.test_snapshot().submitted,
            true,
        );
        let reservation = checked(context, table.reserve())?;
        let target = reservation.fd();
        fixture.path.owner.publish(
            1,
            Job::FileDupExact {
                table: table.clone(),
                source: reused,
                target,
            },
        );
        until("native reserved dup did not return", || {
            fixture.path.owner.observer().results.lock()[1].is_some()
        })?;
        let result = fixture.path.owner.observer().results.lock()[1]
            .take()
            .ok_or(TestError("reserved dup result absent"))?;
        context.expect_eq(result.err(), Some(FsError::Busy));
        context.expect_eq(table.lookup(target).err(), Some(FsError::BadFd));
        drop(reservation);
        fixture.path.owner.publish(
            2,
            Job::FileDupExact {
                table: table.clone(),
                source: reused,
                target,
            },
        );
        until("native exact dup did not return", || {
            fixture.path.owner.observer().results.lock()[2].is_some()
        })?;
        let result = fixture.path.owner.observer().results.lock()[2]
            .take()
            .ok_or(TestError("exact dup result absent"))?;
        let Returned::FileDescriptor(returned_fd) = checked(context, result)? else {
            return Err(TestError("exact dup returned wrong kind"));
        };
        context.expect_eq(returned_fd, target);
        context.expect_eq(
            Arc::ptr_eq(
                &checked(context, table.lookup(reused))?,
                &checked(context, table.lookup(target))?,
            ),
            true,
        );
        fixture
            .path
            .owner
            .observer()
            .hold_loader
            .store(false, Ordering::Release);
        fixture.path.owner.observer().gate.complete();
        fixture.path.owner.join_tasks()?;
        let result = fixture.path.owner.observer().results.lock()[0]
            .take()
            .ok_or(TestError("held file read result absent"))?;
        let Returned::FileRead { bytes, valid } = checked(context, result)? else {
            return Err(TestError("file read returned wrong kind"));
        };
        context.expect_eq(bytes, mm::PAGE_SIZE);
        context.expect_eq(valid, true);
        context.expect_eq(
            checked(context, table.seek(reused, SeekFrom::Current(0)))?,
            9,
        );
        context.diagnostic(format_args!("file-reuse old-read-pattern=1 close-reopen-same-fd=1 other-file-real-IO=1 reserved-native-Busy=1 simultaneous-hardware-IO-covered=0 hardware-EIO-covered=0"));
        Ok(())
    }
}

#[ktest::suite(fixture = FileOwner, suite_resources = DeviceInventory, context = "Thread", tags("fs-file", "fs-file-prefix"), resources("root"), timeout_ms = 30000)]
pub(crate) mod prefix {
    use super::*;

    /// 原映射错误经描述符保留正前缀，后次 Corrupt 不改变位置，零长和 EOF 不触发坏页。
    #[test]
    #[ktest(tags("B22"))]
    fn successful_prefix_preserves_cursor_and_error(
        context: &mut TestContext<'_>,
        fixture: &mut FileOwner,
    ) -> TestResult {
        fixture.prepare(context, 2)?;
        let fd = fixture.open(context, b"/pattern", OpenOptions::default())?;
        let table = fixture.table();
        let mut bytes = [0xa7; 16384];
        context.expect_eq(checked(context, table.read(fd, &mut bytes))?, 12288);
        context.expect_eq(patterned(&bytes[..12288], 0), true);
        context.expect_eq(bytes[12288..].iter().all(|byte| *byte == 0xa7), true);
        context.expect_eq(
            checked(context, table.seek(fd, SeekFrom::Current(0)))?,
            12288,
        );
        context.expect_eq(
            table.read(fd, &mut bytes[..4096]).err(),
            Some(FsError::Corrupt),
        );
        context.expect_eq(
            checked(context, table.seek(fd, SeekFrom::Current(0)))?,
            12288,
        );
        context.expect_eq(checked(context, table.read(fd, &mut []))?, 0);
        context.expect_eq(
            checked(context, table.seek(fd, SeekFrom::End(0)))?,
            PATTERN_SIZE,
        );
        context.expect_eq(checked(context, table.read(fd, &mut bytes[..1]))?, 0);
        context.diagnostic(format_args!("file-prefix delivered=12288 subsequent-error=Corrupt cursor-after-error=12288 hardware-EIO-covered=0"));
        Ok(())
    }
}

#[ktest::suite(fixture = FileOwner, suite_resources = DeviceInventory, context = "Thread", tags("fs-file", "fs-file-mount"), resources("root", "extra"), timeout_ms = 90000)]
pub(crate) mod mounts {
    use super::*;

    /// 第二盘打开文件与 lookup 取得的强引用分别阻止卸载，末引用释放后可正常卸载并清理缓存。
    #[test]
    #[ktest(tags("B20", "B21", "B23"))]
    fn open_and_lookup_hold_mount_until_last_put(
        context: &mut TestContext<'_>,
        fixture: &mut FileOwner,
    ) -> TestResult {
        fixture.prepare(context, 2)?;
        let vfs = fixture.path.vfs();
        let covered = checked(
            context,
            vfs.resolve(fixture.path.context(), b"/mnt/extra", ResolveMode::Follow),
        )?;
        let disk = fixture
            .path
            .owner
            .disks
            .get(1)
            .ok_or(TestError("file extra Disk absent"))?;
        let volume = checked(context, Ext4Volume::open(disk.clone(), 64))?;
        checked(context, vfs.mount(&covered, volume.clone()))?;
        let fd = fixture.open(context, b"/mnt/extra/pattern", OpenOptions::default())?;
        let table = fixture.table();
        read_pattern(context, table, fd, 0, 31)?;
        context.expect_eq(vfs.unmount(&covered).err(), Some(FsError::Busy));
        let file = checked(context, table.lookup(fd))?;
        checked(context, table.close(fd))?;
        context.expect_eq(vfs.unmount(&covered).err(), Some(FsError::Busy));
        let mut bytes = [0xa7; 37];
        context.expect_eq(checked(context, file.read(&mut bytes))?, 37);
        context.expect_eq(patterned(&bytes, 31), true);
        drop(file);
        checked(context, vfs.unmount(&covered))?;
        vfs.prune_unused();
        context.expect_eq(
            vfs.resolve(
                fixture.path.context(),
                b"/mnt/extra/pattern",
                ResolveMode::Follow,
            )
            .err(),
            Some(FsError::NotFound),
        );
        drop(volume);
        Ok(())
    }
}

#[ktest::suite(fixture = FileOwner, suite_resources = DeviceInventory, context = "Thread", min_cpus = 2, tags("fs-file", "fs-oom", "fs-file-oom"), resources("root"), timeout_ms = 90000)]
pub(crate) mod oom {
    use super::*;

    /// CPU0 真堆压力下既有引用发表和热页命中不分配，新打开失败撤槽并在放压后恢复。
    #[test]
    #[ktest(tags("B20", "B21", "B22", "B23"))]
    fn real_pressure_existing_file_and_open_recovery(
        context: &mut TestContext<'_>,
        fixture: &mut FileOwner,
    ) -> TestResult {
        fixture.prepare(context, 3)?;
        let fd = fixture.open(context, b"/pattern", OpenOptions::default())?;
        let table = fixture.table().clone();
        read_pattern(context, &table, fd, 0, mm::PAGE_SIZE)?;
        let cached = checked(context, table.lookup(fd))?;
        let pressure = fixture
            .path
            .owner
            .pressure
            .take()
            .ok_or(TestError("file heap pressure absent"))?;
        fixture.path.owner.publish(
            0,
            Job::FileOom {
                table: table.clone(),
                vfs: fixture.path.vfs().clone(),
                context: fixture.path.context().clone(),
                fd,
                cached,
                pathname: b"/pattern",
                pressure,
            },
        );
        fixture.path.owner.join_tasks()?;
        let result = fixture.path.owner.observer().results.lock()[0]
            .take()
            .ok_or(TestError("file OOM result absent"))?;
        let Returned::FileOom {
            producer_cpu,
            evidence,
            same_ofd,
            bytes,
            valid,
            new_error,
            reserved_fd,
            reused_fd,
            submitted_before,
            submitted_after,
        } = checked(context, result)?
        else {
            return Err(TestError("file OOM returned wrong kind"));
        };
        context.expect_eq(producer_cpu, 0);
        context.expect_eq(evidence, (true, true));
        context.expect_eq(same_ofd, true);
        context.expect_eq(bytes, mm::PAGE_SIZE);
        context.expect_eq(valid, true);
        context.expect_eq(new_error, Some(FsError::NoMemory));
        context.expect_eq(reserved_fd, 1);
        context.expect_eq(reused_fd, reserved_fd);
        context.expect_eq(submitted_after, submitted_before);
        context.expect_eq(
            checked(context, table.seek(fd, SeekFrom::Current(0)))?,
            mm::PAGE_SIZE as u64,
        );
        let reopened = fixture.open(context, b"/pattern", OpenOptions::default())?;
        context.expect_eq(reopened, reused_fd);
        read_pattern(context, &table, reopened, 0, 47)?;
        read_pattern(context, &table, fd, mm::PAGE_SIZE as u64, 43)?;
        context.expect_eq(
            Arc::ptr_eq(
                &checked(context, table.lookup(fd))?,
                &checked(context, table.lookup(reopened))?,
            ),
            false,
        );
        Ok(())
    }
}
