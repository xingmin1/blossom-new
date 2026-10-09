//! 路径保活名字与挂载位置；逐组件查找和 I/O 不跨挂载表短锁。

use alloc::borrow::Cow;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::block::AccessMode;
use crate::fs::{
    DentryCache, DentryRef, DirectoryWriteGuard, Ext4Volume, FsError, InodeCache, InodeKind,
    InodeMetadata, InodeRef,
};
use crate::sync::SpinLock;

/// 只控制没有尾随斜杠的最终符号链接，中间链接始终跟随。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolveMode {
    Follow,
    NoFollow,
}

/// 路径身份由挂载实例和名字对象组成；挂载保活其共享名字索引。
#[derive(Clone)]
pub struct Path {
    mount: Arc<Mount>,
    dentry: DentryRef,
}

/// 挂载只选择卷和访问模式，同时保活域共享的名字索引。
/// 子挂载单向保活父视图，父挂载不反持子挂载。
struct Mount {
    volume: Arc<Ext4Volume>,
    mode: AccessMode,
    dcache: Arc<DentryCache>,
    root: DentryRef,
    covered: Option<Path>,
}

/// 绝对路径与相对路径共享的 root/cwd 边界和进程级文件创建掩码。
#[derive(Clone)]
pub struct FsContext {
    root: Path,
    cwd: Path,
    umask: u16,
}

/// 父查找保留最终原始组件，点组件由真实路径行走解释。
#[derive(Clone)]
pub enum LastComponent {
    Name(Arc<Vec<u8>>),
    Dot,
    DotDot,
    Root,
}

/// 最终名字尚未查找时的实际父视图和原路径尾部语义。
#[derive(Clone)]
pub struct LookupParent {
    pub parent: Path,
    pub component: LastComponent,
    pub trailing_slash: bool,
}

/// 同一 canonical inode 域的视图共享有限名字索引。
/// ponytail: 有限挂载预算内线性查找，预算显著增大时改用有限散列表。
pub struct Vfs {
    dcache: Arc<DentryCache>,
    inodes: Arc<InodeCache>,
    // 根挂载固定占第一个槽；所有向下穿越与卸载检查共用这把短锁。
    mounts: SpinLock<Vec<Arc<Mount>>>,
    mount_capacity: usize,
}

impl Path {
    /// 借用稳定名字对象，名字缓存淘汰不改变当前路径身份。
    pub fn dentry(&self) -> &DentryRef {
        &self.dentry
    }

    /// 等待真实查找结果并借用共享 inode，不建立另一份文件页索引。
    pub fn inode(&self) -> Result<&InodeRef, FsError> {
        self.dentry.inode()
    }

    /// 同时比较挂载实例和名字实例，同一目录在不同视图中的位置不能混同。
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.mount, &other.mount) && self.dentry.ptr_eq(&other.dentry)
    }

    /// 借用该挂载保活的实际卷，文件读取继续使用 inode 自己的 mapping。
    pub fn filesystem(&self) -> &Arc<Ext4Volume> {
        &self.mount.volume
    }

    /// 借用原挂载，为同卷零链接 inode 构造未入名字索引的路径，供打开实例保有原身份。
    /// 调用者须在可睡眠任务中持合法 canonical inode；本操作不创建名字或替换挂载根。
    pub(crate) fn unhashed_inode(&self, inode: InodeRef) -> Result<Self, FsError> {
        if !Arc::ptr_eq(inode.filesystem(), &self.mount.volume) {
            return Err(FsError::InvalidOperation);
        }
        let dentry = DentryRef::try_unhashed(inode)?;
        Ok(Self {
            mount: self.mount.clone(),
            dentry,
        })
    }

    /// 先检查挂载、卷与设备写能力，再按当前 canonical owner/group/other mode 检查访问。
    pub fn check_access(&self, uid: u32, gid: u32, mask: u16) -> Result<(), FsError> {
        if mask & !7 != 0 {
            return Err(FsError::InvalidOperation);
        }
        if mask & 2 != 0 {
            self.check_writable()?;
        }
        let data = self.inode()?.metadata_snapshot()?;
        if uid == 0 {
            if mask & 1 != 0 && data.kind != InodeKind::Directory && data.mode & 0o111 == 0 {
                return Err(FsError::Permission);
            }
            return Ok(());
        }
        let shift = if uid == data.uid {
            6
        } else if gid == data.gid {
            3
        } else {
            0
        };
        if (data.mode >> shift) & mask == mask {
            Ok(())
        } else {
            Err(FsError::Permission)
        }
    }

    /// 检查此路径的独立挂载写能力；不读取 inode 属性，也不授予尚未实现的文件操作。
    pub(crate) fn check_writable(&self) -> Result<(), FsError> {
        let volume = &self.mount.volume;
        if self.mount.mode != AccessMode::ReadWrite
            || volume.mode() != AccessMode::ReadWrite
            || volume.disk().limits().readonly
        {
            return Err(FsError::ReadOnly);
        }
        Ok(())
    }

    /// 返回一个真实祖先边；挂载根的上一个位置是父视图覆盖点。
    fn topology_parent(&self) -> Option<Self> {
        if self.dentry.ptr_eq(&self.mount.root) {
            return self.mount.covered.clone();
        }
        self.dentry.parent().map(|parent| Self {
            mount: self.mount.clone(),
            dentry: parent,
        })
    }

    /// 从挂载根退回覆盖点后再取普通父目录，途中遇到本次 root 就停止。
    fn parent_within(&self, root: &Self) -> Self {
        let mut current = self.clone();
        loop {
            if current.ptr_eq(root) {
                return current;
            }
            let at_mount_root = current.dentry.ptr_eq(&current.mount.root);
            let Some(parent) = current.topology_parent() else {
                return current;
            };
            if !at_mount_root {
                return parent;
            }
            current = parent;
        }
    }
}

impl FsContext {
    /// 验证两个真实目录及 root 祖先关系，以默认 022 初始化文件创建掩码。
    pub fn new(root: Path, cwd: Path) -> Result<Self, FsError> {
        require_directory(&root)?;
        require_directory(&cwd)?;
        let mut ancestor = cwd.clone();
        loop {
            if ancestor.ptr_eq(&root) {
                return Ok(Self {
                    root,
                    cwd,
                    umask: 0o022,
                });
            }
            ancestor = ancestor
                .topology_parent()
                .ok_or(FsError::InvalidOperation)?;
        }
    }

    /// 借用本次查找的绝对路径与父目录边界。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 借用相对路径的起始目录。
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
    /// 返回当前进程文件创建掩码快照。
    pub fn umask(&self) -> u16 {
        self.umask
    }

    /// 在共享 FsContext 锁内替换掩码，并返回旧值。
    pub fn exchange_umask(&mut self, mask: u16) -> u16 {
        core::mem::replace(&mut self.umask, mask & 0o777)
    }

    /// 只替换 cwd；调用方先在锁外验证新 Path，并在锁外释放旧 Path。
    pub fn exchange_cwd(&mut self, cwd: Path) -> Path {
        core::mem::replace(&mut self.cwd, cwd)
    }
}

impl Vfs {
    /// 按明确挂载模式复用 canonical 根与名字索引，并预留包含根挂载的全部有限登记槽。
    pub fn try_new(
        volume: Arc<Ext4Volume>,
        inodes: Arc<InodeCache>,
        dentry_capacity: usize,
        mount_capacity: usize,
        root_mode: AccessMode,
    ) -> Result<Self, FsError> {
        if mount_capacity == 0 {
            return Err(FsError::InvalidOperation);
        }
        if root_mode == AccessMode::ReadWrite && volume.mode() != AccessMode::ReadWrite {
            return Err(FsError::ReadOnly);
        }
        if dentry_capacity == 0 {
            return Err(FsError::InvalidOperation);
        }
        let mut mounts = Vec::new();
        mounts
            .try_reserve_exact(mount_capacity)
            .map_err(|_| FsError::NoMemory)?;
        let (dcache, root) = inodes.dentry_namespace(&volume, dentry_capacity)?;
        let mount = Arc::try_new(Mount {
            volume,
            mode: root_mode,
            dcache: dcache.clone(),
            root,
            covered: None,
        })
        .map_err(|_| FsError::NoMemory)?;
        mounts.push(mount);
        Ok(Self {
            dcache,
            inodes,
            mounts: SpinLock::new(mounts),
            mount_capacity,
        })
    }

    /// 在挂载表短锁中取得根父视图，返回的路径保留这个明确的挂载位置。
    pub fn root(&self) -> Path {
        let mounts = self.mounts.lock();
        let mount = &mounts[0];
        Path {
            mount: mount.clone(),
            dentry: mount.root.clone(),
        }
    }

    /// 借用当前 VFS 的有限名字缓存。
    pub fn dcache(&self) -> &DentryCache {
        self.dcache.as_ref()
    }

    /// 借用所有卷共用的唯一 inode 缓存域。
    pub fn inodes(&self) -> &Arc<InodeCache> {
        &self.inodes
    }

    /// 内核从 cwd 无凭据行走，与显式 dirfd 查找共用同一逐组件算法。
    pub fn resolve(
        &self,
        context: &FsContext,
        pathname: &[u8],
        mode: ResolveMode,
    ) -> Result<Path, FsError> {
        self.resolve_from(context, context.cwd(), pathname, mode, None)
    }

    /// 绝对路径从 root、相对路径从显式起点行走，每个组件都重新检查目录搜索权限。
    pub fn resolve_from(
        &self,
        context: &FsContext,
        start: &Path,
        pathname: &[u8],
        mode: ResolveMode,
        credentials: Option<(u32, u32)>,
    ) -> Result<Path, FsError> {
        let mut links = 0;
        self.resolve_from_with_links(context, start, pathname, mode, credentials, &mut links)
    }

    /// 在共享链接预算内执行逐组件REF-walk，供完整路径与父路径查找共用。
    fn resolve_from_with_links(
        &self,
        context: &FsContext,
        start: &Path,
        pathname: &[u8],
        mode: ResolveMode,
        credentials: Option<(u32, u32)>,
        links: &mut usize,
    ) -> Result<Path, FsError> {
        if pathname.is_empty() {
            return Err(FsError::NotFound);
        }
        if pathname.contains(&0) {
            return Err(FsError::InvalidName);
        }
        self.validate_start(context, start, pathname[0] == b'/')?;
        let mut current = if pathname[0] == b'/' {
            context.root.clone()
        } else {
            start.clone()
        };
        let mut pending = Cow::Borrowed(pathname);
        let mut offset = 0;
        while offset < pending.len() {
            // 先验证当前目录，再处理点组件；file/.. 和 missing/.. 不做词法折叠。
            require_directory(&current)?;
            if let Some((uid, gid)) = credentials {
                current.check_access(uid, gid, 1)?;
            }
            while offset < pending.len() && pending[offset] == b'/' {
                offset += 1;
            }
            if offset == pending.len() {
                return Ok(current);
            }
            let end = pending[offset..]
                .iter()
                .position(|byte| *byte == b'/')
                .map_or(pending.len(), |length| offset + length);
            let name = &pending[offset..end];
            if name == b"." {
                offset = end;
                continue;
            }
            if name == b".." {
                current = self.follow_mounts(current.parent_within(&context.root));
                offset = end;
                continue;
            }
            let dentry = current
                .mount
                .dcache
                .lookup(&self.inodes, &current.dentry, name)?;
            let next = self.follow_mounts(Path {
                mount: current.mount.clone(),
                dentry,
            });
            let inode = next.inode()?;
            let follow = inode.metadata_snapshot()?.kind == InodeKind::Symlink
                && (mode == ResolveMode::Follow || end < pending.len());
            if follow {
                if *links == 40 {
                    return Err(FsError::TooManySymlinks);
                }
                *links += 1;
                let mut target = inode.read_link()?;
                if target.is_empty() {
                    return Err(FsError::NotFound);
                }
                let absolute = target.first() == Some(&b'/');
                // 保留原斜杠后缀，链接目标或原路径的尾斜杠仍要求真实目录。
                target
                    .try_reserve_exact(pending.len() - end)
                    .map_err(|_| FsError::NoMemory)?;
                target.extend_from_slice(&pending[end..]);
                pending = Cow::Owned(target);
                offset = 0;
                if absolute {
                    current = context.root.clone();
                }
            } else {
                current = next;
                offset = end;
            }
        }
        Ok(current)
    }

    /// 沿同一REF-walk解析父路径，保留末组件原字节、特殊点组件和尾斜杠。
    pub fn lookup_parent_from(
        &self,
        context: &FsContext,
        start: &Path,
        pathname: &[u8],
        credentials: Option<(u32, u32)>,
    ) -> Result<LookupParent, FsError> {
        let mut links = 0;
        self.lookup_parent_with_links(context, start, pathname, credentials, &mut links)
    }

    /// 只在最后组件处停止，前缀中的链接跟随计入调用者共享的40次预算。
    fn lookup_parent_with_links(
        &self,
        context: &FsContext,
        start: &Path,
        pathname: &[u8],
        credentials: Option<(u32, u32)>,
        links: &mut usize,
    ) -> Result<LookupParent, FsError> {
        if pathname.is_empty() {
            return Err(FsError::NotFound);
        }
        if pathname.contains(&0) {
            return Err(FsError::InvalidName);
        }
        let trailing_slash = pathname.last() == Some(&b'/');
        let mut end = pathname.len();
        while 0 < end && pathname[end - 1] == b'/' {
            end -= 1;
        }
        if end == 0 {
            self.validate_start(context, start, true)?;
            require_directory(&context.root)?;
            if let Some((uid, gid)) = credentials {
                context.root.check_access(uid, gid, 1)?;
            }
            return Ok(LookupParent {
                parent: context.root.clone(),
                component: LastComponent::Root,
                trailing_slash: true,
            });
        }

        let start_at = pathname[..end]
            .iter()
            .rposition(|byte| *byte == b'/')
            .map_or(0, |position| position + 1);
        let raw_name = &pathname[start_at..end];
        let prefix = &pathname[..start_at];
        let parent = if prefix.is_empty() {
            self.validate_start(context, start, false)?;
            require_directory(start)?;
            if let Some((uid, gid)) = credentials {
                start.check_access(uid, gid, 1)?;
            }
            start.clone()
        } else {
            self.resolve_from_with_links(
                context,
                start,
                prefix,
                ResolveMode::Follow,
                credentials,
                links,
            )?
        };
        let component = if raw_name == b"." {
            LastComponent::Dot
        } else if raw_name == b".." {
            LastComponent::DotDot
        } else {
            LastComponent::Name(try_raw_name(raw_name)?)
        };
        Ok(LookupParent {
            parent,
            component,
            trailing_slash,
        })
    }

    /// 根与相对起点必须仍属于当前挂载表；绝对路径忽略传入的 dirfd。
    fn validate_start(
        &self,
        context: &FsContext,
        start: &Path,
        absolute: bool,
    ) -> Result<(), FsError> {
        let mounts = self.mounts.lock();
        if !contains_mount(&mounts, &context.root.mount)
            || (!absolute && !contains_mount(&mounts, &start.mount))
        {
            return Err(FsError::InvalidOperation);
        }
        Ok(())
    }

    /// 父目录写 guard 内为真实现存 inode 取得当前名字对象。
    fn existing_child_locked(
        &self,
        parent: &Path,
        name: Arc<Vec<u8>>,
        inode: InodeRef,
    ) -> Result<Path, FsError> {
        if let Some(dentry) = parent
            .mount
            .dcache
            .cached_locked(&parent.dentry, name.as_slice())
        {
            match dentry.inode() {
                Ok(cached) if cached.ptr_eq(&inode) => {
                    return Ok(Path {
                        mount: parent.mount.clone(),
                        dentry,
                    });
                }
                Ok(_) => return Err(FsError::InvalidOperation),
                Err(FsError::NotFound) => {}
                Err(error) => return Err(error),
            }
        }
        let prepared = parent
            .mount
            .dcache
            .try_prepare_positive(&parent.dentry, name, inode)?;
        let dentry = prepared.dentry().clone();
        prepared.publish();
        Ok(Path {
            mount: parent.mount.clone(),
            dentry,
        })
    }

    /// 在挂载表短锁内判断根或被子挂载覆盖的 dentry。
    fn is_mountpoint(&self, path: &Path) -> bool {
        if path.dentry.ptr_eq(&path.mount.root) {
            return true;
        }
        self.mounts.lock().iter().any(|mount| {
            mount
                .covered
                .as_ref()
                .is_some_and(|covered| covered.ptr_eq(path))
        })
    }

    /// 解析并打开现有名字，或在最终目录写 guard 下准备文件、OFD 后再发表新名字。
    /// `create_mode` 为 `(requested_mode, umask_snapshot)`；只有实际新建时才交给inode层应用。
    pub fn open_with<T>(
        &self,
        context: &FsContext,
        start: &Path,
        pathname: &[u8],
        credentials: (u32, u32),
        create_mode: Option<(u16, u16)>,
        exclusive: bool,
        no_follow: bool,
        prepare: impl FnOnce(Path, bool) -> Result<T, FsError>,
    ) -> Result<T, FsError> {
        if exclusive && create_mode.is_none() {
            return Err(FsError::InvalidOperation);
        }
        let mut prepare = Some(prepare);
        if create_mode.is_none() {
            let path = self.resolve_from(
                context,
                start,
                pathname,
                if no_follow {
                    ResolveMode::NoFollow
                } else {
                    ResolveMode::Follow
                },
                Some(credentials),
            )?;
            if no_follow && path.inode()?.metadata_snapshot()?.kind == InodeKind::Symlink {
                return Err(FsError::TooManySymlinks);
            }
            return prepare
                .take()
                .expect("open preparation callback was consumed")(path, false);
        }
        let (requested_mode, umask) = create_mode.expect("create mode was checked above");
        let mut current_start = start.clone();
        let mut pending = Cow::Borrowed(pathname);
        let mut links = 0;
        loop {
            let lookup = self.lookup_parent_with_links(
                context,
                &current_start,
                pending.as_ref(),
                Some(credentials),
                &mut links,
            )?;
            if lookup.trailing_slash {
                return Err(FsError::IsDirectory);
            }
            match lookup.component {
                LastComponent::Name(name) => {
                    let parent_inode = lookup.parent.inode()?.clone();
                    let directory = parent_inode.directory_write()?;
                    let parent_metadata = directory.metadata_snapshot();
                    check_access_metadata(
                        &lookup.parent,
                        &parent_metadata,
                        credentials.0,
                        credentials.1,
                        1,
                    )?;
                    if let Some(inode) = directory.lookup(&self.inodes, name.as_slice())? {
                        if exclusive {
                            return Err(FsError::Exists);
                        }
                        let metadata = inode.metadata_snapshot()?;
                        if metadata.kind == InodeKind::Symlink {
                            if no_follow {
                                return Err(FsError::TooManySymlinks);
                            }
                            if links == 40 {
                                return Err(FsError::TooManySymlinks);
                            }
                            links += 1;
                            drop(directory);
                            let target = inode.read_link()?;
                            if target.is_empty() {
                                return Err(FsError::NotFound);
                            }
                            let absolute = target.first() == Some(&b'/');
                            current_start = if absolute {
                                context.root.clone()
                            } else {
                                lookup.parent
                            };
                            pending = Cow::Owned(target);
                            continue;
                        }
                        let path = self.existing_child_locked(&lookup.parent, name, inode)?;
                        drop(directory);
                        return prepare
                            .take()
                            .expect("open preparation callback was consumed")(
                            self.follow_mounts(path),
                            false,
                        );
                    }

                    check_parent_write(
                        &lookup.parent,
                        &parent_metadata,
                        credentials.0,
                        credentials.1,
                    )?;
                    let prepared_dentry = lookup
                        .parent
                        .mount
                        .dcache
                        .try_prepare_child(&lookup.parent.dentry, name.clone())?;
                    if self.inodes.is_full() {
                        lookup.parent.mount.dcache.prune_unused();
                    }
                    let now_ns = crate::time::now().as_nanos();
                    let new_inode = self.inodes.prepare_regular_inode(
                        lookup.parent.filesystem(),
                        &parent_metadata,
                        requested_mode & 0o7777,
                        umask & 0o777,
                        credentials.0,
                        credentials.1,
                        now_ns,
                    )?;
                    let inode = new_inode.inode().clone();
                    let prepared_dentry = prepared_dentry.bind_positive(inode.clone());
                    let path = Path {
                        mount: lookup.parent.mount.clone(),
                        dentry: prepared_dentry.dentry().clone(),
                    };
                    let file = prepare
                        .take()
                        .expect("open preparation callback was consumed")(
                        path, true
                    )?;
                    let edit = directory.prepare_insert(
                        name.as_slice(),
                        inode.number(),
                        InodeKind::Regular,
                        0,
                        now_ns,
                    )?;
                    let _published_inode = new_inode.publish();
                    directory.publish(edit);
                    prepared_dentry.publish();
                    return Ok(file);
                }
                LastComponent::Dot => {
                    if exclusive {
                        return Err(FsError::Exists);
                    }
                    return prepare
                        .take()
                        .expect("open preparation callback was consumed")(
                        lookup.parent, false
                    );
                }
                LastComponent::DotDot => {
                    if exclusive {
                        return Err(FsError::Exists);
                    }
                    let path = self.follow_mounts(lookup.parent.parent_within(&context.root));
                    return prepare
                        .take()
                        .expect("open preparation callback was consumed")(
                        path, false
                    );
                }
                LastComponent::Root => {
                    if exclusive {
                        return Err(FsError::Exists);
                    }
                    return prepare
                        .take()
                        .expect("open preparation callback was consumed")(
                        context.root.clone(),
                        false,
                    );
                }
            }
        }
    }

    /// 在父目录独占 guard 下重查名字，并在目录项可见前准备目录、父链接数和缓存槽。
    pub fn mkdir_from(
        &self,
        context: &FsContext,
        start: &Path,
        pathname: &[u8],
        mode: u16,
        credentials: (u32, u32),
        umask: u16,
    ) -> Result<(), FsError> {
        let lookup = self.lookup_parent_from(context, start, pathname, Some(credentials))?;
        let name = match lookup.component {
            LastComponent::Name(name) => name,
            LastComponent::Dot | LastComponent::DotDot | LastComponent::Root => {
                return Err(FsError::Exists);
            }
        };
        let parent_inode = lookup.parent.inode()?.clone();
        let directory = parent_inode.directory_write()?;
        let parent_metadata = directory.metadata_snapshot();
        if directory.lookup(&self.inodes, name.as_slice())?.is_some() {
            check_access_metadata(
                &lookup.parent,
                &parent_metadata,
                credentials.0,
                credentials.1,
                1,
            )?;
            return Err(FsError::Exists);
        }
        check_parent_write(
            &lookup.parent,
            &parent_metadata,
            credentials.0,
            credentials.1,
        )?;
        let prepared_dentry = lookup
            .parent
            .mount
            .dcache
            .try_prepare_child(&lookup.parent.dentry, name.clone())?;
        if self.inodes.is_full() {
            lookup.parent.mount.dcache.prune_unused();
        }
        let now_ns = crate::time::now().as_nanos();
        let new_inode = self.inodes.prepare_directory_inode(
            lookup.parent.filesystem(),
            &parent_metadata,
            mode & 0o1777,
            umask & 0o777,
            credentials.0,
            credentials.1,
            now_ns,
        )?;
        let inode = new_inode.inode().clone();
        let prepared_dentry = prepared_dentry.bind_positive(inode.clone());
        let edit = directory.prepare_insert(
            name.as_slice(),
            inode.number(),
            InodeKind::Directory,
            1,
            now_ns,
        )?;
        let _published_inode = new_inode.publish();
        directory.publish(edit);
        prepared_dentry.publish();
        Ok(())
    }

    /// 以当前原始目标字节建立短或长符号链接，使用与普通创建相同的父权限和发表边界。
    pub fn symlink_from(
        &self,
        context: &FsContext,
        start: &Path,
        pathname: &[u8],
        target: &[u8],
        credentials: (u32, u32),
    ) -> Result<(), FsError> {
        let lookup = self.lookup_parent_from(context, start, pathname, Some(credentials))?;
        let name = match lookup.component {
            LastComponent::Name(name) => name,
            LastComponent::Dot | LastComponent::DotDot | LastComponent::Root => {
                return Err(FsError::Exists);
            }
        };
        let parent_inode = lookup.parent.inode()?.clone();
        let directory = parent_inode.directory_write()?;
        let parent_metadata = directory.metadata_snapshot();
        if directory.lookup(&self.inodes, name.as_slice())?.is_some() {
            check_access_metadata(
                &lookup.parent,
                &parent_metadata,
                credentials.0,
                credentials.1,
                1,
            )?;
            return Err(FsError::Exists);
        }
        if lookup.trailing_slash {
            return Err(FsError::NotFound);
        }
        check_parent_write(
            &lookup.parent,
            &parent_metadata,
            credentials.0,
            credentials.1,
        )?;
        let prepared_dentry = lookup
            .parent
            .mount
            .dcache
            .try_prepare_child(&lookup.parent.dentry, name.clone())?;
        if self.inodes.is_full() {
            lookup.parent.mount.dcache.prune_unused();
        }
        let now_ns = crate::time::now().as_nanos();
        let new_inode = self.inodes.prepare_symlink_inode(
            lookup.parent.filesystem(),
            &parent_metadata,
            target,
            credentials.0,
            credentials.1,
            now_ns,
        )?;
        let inode = new_inode.inode().clone();
        let prepared_dentry = prepared_dentry.bind_positive(inode.clone());
        let edit = directory.prepare_insert(
            name.as_slice(),
            inode.number(),
            InodeKind::Symlink,
            0,
            now_ns,
        )?;
        let _published_inode = new_inode.publish();
        directory.publish(edit);
        prepared_dentry.publish();
        Ok(())
    }

    /// 只移除父目录下重查到的名字；旧 Path 与 OFD 保留原 canonical inode。
    pub fn unlink_from(
        &self,
        context: &FsContext,
        start: &Path,
        pathname: &[u8],
        credentials: (u32, u32),
    ) -> Result<(), FsError> {
        let lookup = self.lookup_parent_from(context, start, pathname, Some(credentials))?;
        let name = match lookup.component {
            LastComponent::Name(name) => name,
            LastComponent::Dot | LastComponent::DotDot | LastComponent::Root => {
                return Err(FsError::IsDirectory);
            }
        };
        let parent_inode = lookup.parent.inode()?.clone();
        let directory = parent_inode.directory_write()?;
        let parent_metadata = directory.metadata_snapshot();
        lookup.parent.check_writable()?;
        let inode = directory
            .lookup(&self.inodes, name.as_slice())?
            .ok_or(FsError::NotFound)?;
        let metadata = inode.metadata_snapshot()?;
        if lookup.trailing_slash {
            return Err(if metadata.kind == InodeKind::Directory {
                FsError::IsDirectory
            } else {
                FsError::NotDirectory
            });
        }
        check_parent_permissions(&parent_metadata, credentials.0, credentials.1)?;
        check_sticky_directory(&parent_metadata, &metadata, credentials.0)?;
        if metadata.kind == InodeKind::Directory {
            return Err(FsError::IsDirectory);
        }
        let target = self.existing_child_locked(&lookup.parent, name.clone(), inode.clone())?;
        if self.is_mountpoint(&target) {
            return Err(FsError::Busy);
        }
        let child = inode.inode_write()?;
        let child_metadata = child.metadata_snapshot();
        if child_metadata.links == 0 {
            return Err(FsError::NotFound);
        }
        let now_ns = crate::time::now().as_nanos();
        let child_edit = child.prepare_link_delta(-1, now_ns)?;
        let directory_edit = directory.prepare_remove(name.as_slice(), 0, now_ns)?;
        child.publish_metadata(child_edit);
        directory.publish(directory_edit);
        target.mount.dcache.unhash(&target.dentry);
        Ok(())
    }

    /// 在目标父目录写 guard 下建立同一 canonical 非目录 inode 的第二个名字。
    pub fn link_from(
        &self,
        source: &Path,
        context: &FsContext,
        start: &Path,
        pathname: &[u8],
        credentials: (u32, u32),
    ) -> Result<(), FsError> {
        let lookup = self.lookup_parent_from(context, start, pathname, Some(credentials))?;
        let name = match lookup.component {
            LastComponent::Name(name) => name,
            LastComponent::Dot | LastComponent::DotDot | LastComponent::Root => {
                return Err(FsError::Exists);
            }
        };
        let source_inode = source.inode()?.clone();
        let parent_inode = lookup.parent.inode()?.clone();
        let directory = parent_inode.directory_write()?;
        let parent_metadata = directory.metadata_snapshot();
        if directory.lookup(&self.inodes, name.as_slice())?.is_some() {
            check_access_metadata(
                &lookup.parent,
                &parent_metadata,
                credentials.0,
                credentials.1,
                1,
            )?;
            return Err(FsError::Exists);
        }
        if lookup.trailing_slash {
            return Err(FsError::NotFound);
        }
        check_parent_write(
            &lookup.parent,
            &parent_metadata,
            credentials.0,
            credentials.1,
        )?;
        if !Arc::ptr_eq(&source.mount, &lookup.parent.mount) {
            return Err(FsError::CrossDevice);
        }
        // inode kind 不可由属性更新修改；先读稳定类型，避免 source 与父目录相同时递归取得写锁。
        let source_kind = source_inode.metadata_snapshot()?.kind;
        if source_kind == InodeKind::Directory {
            return Err(FsError::NotPermitted);
        }
        let source_guard = source_inode.inode_write()?;
        let source_metadata = source_guard.metadata_snapshot();
        if source_metadata.links == 0 {
            return Err(FsError::NotFound);
        }
        let prepared_dentry = lookup
            .parent
            .mount
            .dcache
            .try_prepare_child(&lookup.parent.dentry, name.clone())?;
        let now_ns = crate::time::now().as_nanos();
        let source_edit = source_guard.prepare_link_delta(1, now_ns)?;
        let directory_edit = directory.prepare_insert(
            name.as_slice(),
            source_inode.number(),
            source_metadata.kind,
            0,
            now_ns,
        )?;
        let prepared_dentry = prepared_dentry.bind_positive(source_inode.clone());
        source_guard.publish_metadata(source_edit);
        directory.publish(directory_edit);
        prepared_dentry.publish();
        Ok(())
    }

    /// 仅真实空目录可摘除；旧 cwd 与 Path 保留零链接目录对象。
    pub fn rmdir_from(
        &self,
        context: &FsContext,
        start: &Path,
        pathname: &[u8],
        credentials: (u32, u32),
    ) -> Result<(), FsError> {
        let lookup = self.lookup_parent_from(context, start, pathname, Some(credentials))?;
        let name = match lookup.component {
            LastComponent::Name(name) => name,
            LastComponent::Dot => return Err(FsError::InvalidOperation),
            LastComponent::DotDot => return Err(FsError::NotEmpty),
            LastComponent::Root => return Err(FsError::Busy),
        };
        let parent_inode = lookup.parent.inode()?.clone();
        let directory = parent_inode.directory_write()?;
        let parent_metadata = directory.metadata_snapshot();
        lookup.parent.check_writable()?;
        let inode = directory
            .lookup(&self.inodes, name.as_slice())?
            .ok_or(FsError::NotFound)?;
        let metadata = inode.metadata_snapshot()?;
        check_parent_permissions(&parent_metadata, credentials.0, credentials.1)?;
        check_sticky_directory(&parent_metadata, &metadata, credentials.0)?;
        if metadata.kind != InodeKind::Directory {
            return Err(FsError::NotDirectory);
        }
        let target = self.existing_child_locked(&lookup.parent, name.clone(), inode.clone())?;
        let child = inode.directory_write()?;
        if self.is_mountpoint(&target) {
            return Err(FsError::Busy);
        }
        let child_metadata = child.metadata_snapshot();
        if child_metadata.links == 0 {
            return Err(FsError::NotFound);
        }
        if !child.is_empty()? {
            return Err(FsError::NotEmpty);
        }
        let now_ns = crate::time::now().as_nanos();
        let child_edit = child.prepare_link_delta(-2, now_ns)?;
        let directory_edit = directory.prepare_remove(name.as_slice(), -1, now_ns)?;
        child.publish_metadata(child_edit);
        directory.publish(directory_edit);
        target.mount.dcache.unhash(&target.dentry);
        Ok(())
    }

    /// 按同一挂载的原生目录操作移动名字；NOREPLACE与普通rename共享一次父锁协议。
    pub fn rename_from(
        &self,
        context: &FsContext,
        old_start: &Path,
        old_pathname: &[u8],
        new_start: &Path,
        new_pathname: &[u8],
        noreplace: bool,
        credentials: (u32, u32),
    ) -> Result<(), FsError> {
        let old = self.lookup_parent_from(context, old_start, old_pathname, Some(credentials))?;
        let new = self.lookup_parent_from(context, new_start, new_pathname, Some(credentials))?;
        if !Arc::ptr_eq(&old.parent.mount, &new.parent.mount) {
            return Err(FsError::CrossDevice);
        }
        let old_name = match &old.component {
            LastComponent::Name(name) => name.clone(),
            LastComponent::Dot | LastComponent::DotDot | LastComponent::Root => {
                return Err(FsError::Busy);
            }
        };
        let new_name = match &new.component {
            LastComponent::Name(name) => name.clone(),
            LastComponent::Dot | LastComponent::DotDot | LastComponent::Root => {
                return Err(if noreplace {
                    FsError::Exists
                } else {
                    FsError::Busy
                });
            }
        };
        let same_parent = old.parent.ptr_eq(&new.parent);
        let _rename_guard = if same_parent {
            None
        } else {
            Some(old.parent.filesystem().lock_rename())
        };
        let source_first = same_parent
            || is_path_within(&old.parent, &new.parent)
            || !is_path_within(&new.parent, &old.parent);
        let old_parent_inode = old.parent.inode()?.clone();
        let new_parent_inode = new.parent.inode()?.clone();
        if same_parent {
            let directory = old_parent_inode.directory_write()?;
            return self.rename_locked(
                &old,
                &new,
                old_name,
                new_name,
                noreplace,
                credentials,
                &directory,
                None,
            );
        }
        if source_first {
            let source_directory = old_parent_inode.directory_write()?;
            let target_directory = new_parent_inode.directory_write()?;
            self.rename_locked(
                &old,
                &new,
                old_name,
                new_name,
                noreplace,
                credentials,
                &source_directory,
                Some(&target_directory),
            )
        } else {
            let target_directory = new_parent_inode.directory_write()?;
            let source_directory = old_parent_inode.directory_write()?;
            self.rename_locked(
                &old,
                &new,
                old_name,
                new_name,
                noreplace,
                credentials,
                &source_directory,
                Some(&target_directory),
            )
        }
    }

    /// 父锁已按目录祖先关系取得；此处完成真实重查、所有可失败准备与无失败提交。
    fn rename_locked(
        &self,
        old: &LookupParent,
        new: &LookupParent,
        old_name: Arc<Vec<u8>>,
        new_name: Arc<Vec<u8>>,
        noreplace: bool,
        credentials: (u32, u32),
        source_parent: &DirectoryWriteGuard<'_>,
        target_parent: Option<&DirectoryWriteGuard<'_>>,
    ) -> Result<(), FsError> {
        let same_parent = target_parent.is_none();
        let destination_parent = target_parent.unwrap_or(source_parent);
        let old_metadata = source_parent.metadata_snapshot();
        let new_metadata = destination_parent.metadata_snapshot();
        check_access_metadata(&old.parent, &old_metadata, credentials.0, credentials.1, 1)?;
        check_access_metadata(&new.parent, &new_metadata, credentials.0, credentials.1, 1)?;
        old.parent.check_writable()?;
        let source_inode = source_parent
            .lookup(&self.inodes, old_name.as_slice())?
            .ok_or(FsError::NotFound)?;
        let target_inode = destination_parent.lookup(&self.inodes, new_name.as_slice())?;
        if noreplace && target_inode.is_some() {
            return Err(FsError::Exists);
        }
        let source_metadata = source_inode.metadata_snapshot()?;
        let target_metadata = target_inode
            .as_ref()
            .map(|inode| inode.metadata_snapshot())
            .transpose()?;
        if old.trailing_slash && source_metadata.kind != InodeKind::Directory {
            return Err(FsError::NotDirectory);
        }
        if new.trailing_slash && source_metadata.kind != InodeKind::Directory {
            return Err(FsError::NotDirectory);
        }
        if target_inode
            .as_ref()
            .is_some_and(|target| target.ptr_eq(&source_inode))
        {
            return Ok(());
        }
        check_parent_permissions(&old_metadata, credentials.0, credentials.1)?;
        check_parent_permissions(&new_metadata, credentials.0, credentials.1)?;
        check_sticky_directory(&old_metadata, &source_metadata, credentials.0)?;
        if let Some(metadata) = target_metadata.as_ref() {
            check_sticky_directory(&new_metadata, metadata, credentials.0)?;
        }
        if source_metadata.kind == InodeKind::Directory
            && target_metadata
                .as_ref()
                .is_some_and(|metadata| metadata.kind != InodeKind::Directory)
        {
            return Err(FsError::NotDirectory);
        }
        if source_metadata.kind != InodeKind::Directory
            && target_metadata
                .as_ref()
                .is_some_and(|metadata| metadata.kind == InodeKind::Directory)
        {
            return Err(FsError::IsDirectory);
        }

        // 父目录写侧与卷 rename guard 已固定拓扑；先拒绝祖先重叠，
        // 避免源子目录恰为已持父目录时递归取得同一写锁。
        let source_is_directory = source_metadata.kind == InodeKind::Directory;
        let source_path =
            self.existing_child_locked(&old.parent, old_name.clone(), source_inode.clone())?;
        let target_path = target_inode
            .as_ref()
            .map(|inode| self.existing_child_locked(&new.parent, new_name.clone(), inode.clone()))
            .transpose()?;
        if source_is_directory && is_path_within(&source_path, &new.parent) {
            return Err(FsError::InvalidOperation);
        }
        if target_path
            .as_ref()
            .is_some_and(|path| is_path_within(path, &old.parent))
        {
            return Err(FsError::NotEmpty);
        }
        let source_directory = if source_is_directory {
            Some(source_inode.directory_write()?)
        } else {
            None
        };
        let source_directory_metadata = source_directory
            .as_ref()
            .map(|directory| directory.metadata_snapshot());
        if let Some(metadata) = source_directory_metadata.as_ref() {
            if metadata.links == 0 {
                return Err(FsError::NotFound);
            }
            if !same_parent {
                check_inode_mode_access(metadata, credentials.0, credentials.1, 2)?;
            }
        }
        let target_is_directory = target_metadata
            .as_ref()
            .is_some_and(|metadata| metadata.kind == InodeKind::Directory);
        let (source_file, target_directory, target_file) = if source_is_directory {
            let target_directory = match target_inode.as_ref() {
                Some(inode) if target_is_directory => Some(inode.directory_write()?),
                _ => None,
            };
            let target_file = match target_inode.as_ref() {
                Some(inode) if !target_is_directory => Some(inode.inode_write()?),
                _ => None,
            };
            (None, target_directory, target_file)
        } else if target_is_directory {
            let source_file = Some(source_inode.inode_write()?);
            let target_directory = Some(
                target_inode
                    .as_ref()
                    .expect("directory target has no canonical inode")
                    .directory_write()?,
            );
            (source_file, target_directory, None)
        } else {
            match target_inode.as_ref() {
                Some(target) => {
                    // 父目录 guard 已按祖先偏序取得；两个非目录子 inode 再按卷内 inode 号统一排序。
                    if source_inode.number() < target.number() {
                        let source = Some(source_inode.inode_write()?);
                        let destination = Some(target.inode_write()?);
                        (source, None, destination)
                    } else {
                        let destination = Some(target.inode_write()?);
                        let source = Some(source_inode.inode_write()?);
                        (source, None, destination)
                    }
                }
                None => (Some(source_inode.inode_write()?), None, None),
            }
        };
        let source_current = match source_directory_metadata {
            Some(metadata) => metadata,
            None => source_file
                .as_ref()
                .expect("non-directory source has no inode write guard")
                .metadata_snapshot(),
        };
        if !source_is_directory && source_current.links == 0 {
            return Err(FsError::NotFound);
        }
        let target_current = match target_inode.as_ref() {
            Some(_) => Some(match target_directory.as_ref() {
                Some(directory) => directory.metadata_snapshot(),
                None => target_file
                    .as_ref()
                    .expect("non-directory target has no inode write guard")
                    .metadata_snapshot(),
            }),
            None => None,
        };
        if target_current
            .as_ref()
            .is_some_and(|metadata| metadata.links == 0)
        {
            return Err(FsError::NotFound);
        }
        if self.is_mountpoint(&source_path)
            || target_path
                .as_ref()
                .is_some_and(|path| self.is_mountpoint(path))
        {
            return Err(FsError::Busy);
        }
        if let Some(directory) = target_directory.as_ref() {
            if !directory.is_empty()? {
                return Err(FsError::NotEmpty);
            }
        }

        let now_ns = crate::time::now().as_nanos();
        let source_metadata_edit = if source_is_directory && !same_parent {
            None
        } else if let Some(directory) = source_directory.as_ref() {
            Some(directory.prepare_ctime(now_ns)?)
        } else {
            Some(
                source_file
                    .as_ref()
                    .expect("non-directory source has no inode write guard")
                    .prepare_ctime(now_ns)?,
            )
        };
        let dotdot_edit = if source_is_directory && !same_parent {
            let parent_number = new.parent.inode()?.number();
            Some(
                source_directory
                    .as_ref()
                    .expect("moved directory has no directory guard")
                    .prepare_dotdot(parent_number, now_ns)?,
            )
        } else {
            None
        };
        let target_metadata_edit = match target_current.as_ref() {
            Some(metadata) if metadata.kind == InodeKind::Directory => Some(
                target_directory
                    .as_ref()
                    .expect("directory target has no directory guard")
                    .prepare_link_delta(-2, now_ns)?,
            ),
            Some(_) => Some(
                target_file
                    .as_ref()
                    .expect("non-directory target has no inode guard")
                    .prepare_link_delta(-1, now_ns)?,
            ),
            None => None,
        };
        let target_identity = target_inode
            .as_ref()
            .zip(target_current.as_ref())
            .map(|(inode, metadata)| (inode.number(), metadata.kind));
        let old_parent_links = if source_is_directory && !same_parent {
            -1
        } else {
            0
        };
        let (source_parent_edit, target_parent_edit) = if same_parent {
            let parent_links = if source_is_directory && target_is_directory {
                -1
            } else {
                0
            };
            (
                source_parent.prepare_move(
                    old_name.as_slice(),
                    new_name.as_slice(),
                    source_inode.number(),
                    source_metadata.kind,
                    target_identity,
                    parent_links,
                    now_ns,
                )?,
                None,
            )
        } else {
            let old_edit =
                source_parent.prepare_remove(old_name.as_slice(), old_parent_links, now_ns)?;
            let new_edit = match target_identity {
                Some(_) => destination_parent.prepare_replace(
                    new_name.as_slice(),
                    source_inode.number(),
                    source_metadata.kind,
                    0,
                    now_ns,
                )?,
                None => destination_parent.prepare_insert(
                    new_name.as_slice(),
                    source_inode.number(),
                    source_metadata.kind,
                    if source_is_directory { 1 } else { 0 },
                    now_ns,
                )?,
            };
            (old_edit, Some(new_edit))
        };

        if let Some(edit) = source_metadata_edit {
            if let Some(directory) = source_directory.as_ref() {
                directory.publish_metadata(edit);
            } else {
                source_file
                    .as_ref()
                    .expect("non-directory source has no inode write guard")
                    .publish_metadata(edit);
            }
        }
        if let Some(edit) = target_metadata_edit {
            if let Some(directory) = target_directory.as_ref() {
                directory.publish_metadata(edit);
            } else {
                target_file
                    .as_ref()
                    .expect("non-directory target has no inode write guard")
                    .publish_metadata(edit);
            }
        }
        if let Some(edit) = dotdot_edit {
            source_directory
                .as_ref()
                .expect("moved directory has no directory guard")
                .publish(edit);
        }
        source_parent.publish(source_parent_edit);
        if let Some(edit) = target_parent_edit {
            destination_parent.publish(edit);
        }
        source_path.mount.dcache.move_entry(
            &source_path.dentry,
            target_path.as_ref().map(|path| &path.dentry),
            &new.parent.dentry,
            new_name,
        );
        Ok(())
    }

    /// 按独立模式在父视图目录下挂入卷，准备与 inode I/O 在表锁外，发表时双查。
    pub fn mount(
        &self,
        covered: &Path,
        volume: Arc<Ext4Volume>,
        mode: AccessMode,
    ) -> Result<(), FsError> {
        require_directory(covered)?;
        if mode == AccessMode::ReadWrite && volume.mode() != AccessMode::ReadWrite {
            return Err(FsError::ReadOnly);
        }
        {
            let mounts = self.mounts.lock();
            self.check_mount(&mounts, covered, &volume)?;
        }
        let root = self.inodes.root_dentry(&volume)?;
        let candidate = Arc::try_new(Mount {
            volume,
            mode,
            dcache: self.dcache.clone(),
            root,
            covered: Some(covered.clone()),
        })
        .map_err(|_| FsError::NoMemory)?;
        let inode = covered.inode()?;
        let directory = inode.directory_read()?;
        if directory.metadata_snapshot().links == 0
            || (!covered.dentry.ptr_eq(&covered.mount.root) && !covered.dentry.is_hashed())
        {
            return Err(FsError::NotFound);
        }
        // 目录读 guard 与 rmdir/rename 的子目录写 guard 排除挂载点被同时断开。
        let mut mounts = self.mounts.lock();
        self.check_mount(&mounts, covered, &candidate.volume)?;
        mounts.push(candidate);
        Ok(())
    }

    /// 以父视图覆盖点卸载；活路径或子挂载返回 Busy，成功摘除后的复杂释放在锁外。
    pub fn unmount(&self, covered: &Path) -> Result<(), FsError> {
        let victim = {
            let mut mounts = self.mounts.lock();
            if !contains_mount(&mounts, &covered.mount) {
                return Err(FsError::InvalidOperation);
            }
            let position = mounts
                .iter()
                .position(|mount| {
                    mount
                        .covered
                        .as_ref()
                        .is_some_and(|path| path.ptr_eq(covered))
                })
                .ok_or(FsError::NotFound)?;
            let mount = &mounts[position];
            let has_children = mounts.iter().any(|child| {
                child
                    .covered
                    .as_ref()
                    .is_some_and(|path| Arc::ptr_eq(&path.mount, mount))
            });
            // 目标只有登记表的一份基数引用；每个活 Path 和子挂载各增加一份。
            // 持锁排除新的向下取得；没有现存 Path 时 Clone 不能凭空制造活引用。
            if has_children || Arc::strong_count(mount) != 1 {
                return Err(FsError::Busy);
            }
            mounts.swap_remove(position)
        };
        let volume = victim.volume.clone();
        drop(victim);
        self.dcache.prune_unused();
        self.inodes.prune_unused();
        volume.prune_unused();
        Ok(())
    }

    /// 只回收无人使用的静止缓存；页与 I/O 自己的拥有者仍保活卷和文件系统占用。
    pub fn prune_unused(&self) -> usize {
        let mut removed = self.dcache.prune_unused() + self.inodes.prune_unused();
        let count = self.mounts.lock().len();
        for position in 0..count {
            let volume = {
                let mounts = self.mounts.lock();
                mounts.get(position).map(|mount| mount.volume.clone())
            };
            if let Some(volume) = volume {
                removed += volume.prune_unused();
            }
        }
        removed
    }

    /// 同一短表锁检查父挂载、覆盖点、不同卷实例占用的 Disk 和固定预算。
    fn check_mount(
        &self,
        mounts: &[Arc<Mount>],
        covered: &Path,
        volume: &Arc<Ext4Volume>,
    ) -> Result<(), FsError> {
        if !contains_mount(mounts, &covered.mount) {
            return Err(FsError::InvalidOperation);
        }
        let disk_conflict = mounts.iter().any(|mount| {
            !Arc::ptr_eq(&mount.volume, volume)
                && core::ptr::eq(
                    mount.volume.disk().as_ref().get_ref(),
                    volume.disk().as_ref().get_ref(),
                )
        });
        if find_mount(mounts, &covered.mount, &covered.dentry).is_some() || disk_conflict {
            return Err(FsError::Busy);
        }
        if self.mount_capacity <= mounts.len() {
            return Err(FsError::NoMemory);
        }
        Ok(())
    }

    /// 在同一表锁内找到最上层覆盖挂载并取得引用，旧 Path 在锁外释放。
    fn follow_mounts(&self, path: Path) -> Path {
        let crossed = {
            let mounts = self.mounts.lock();
            let mut mount = &path.mount;
            let mut dentry = &path.dentry;
            while let Some(child) = find_mount(&mounts, mount, dentry) {
                mount = child;
                dentry = &child.root;
            }
            if Arc::ptr_eq(mount, &path.mount) {
                None
            } else {
                Some(Path {
                    mount: mount.clone(),
                    dentry: dentry.clone(),
                })
            }
        };
        crossed.unwrap_or(path)
    }
}

/// 只接受实际正目录，缓存命中和点组件都不能绕过类型检查。
fn require_directory(path: &Path) -> Result<(), FsError> {
    if path.inode()?.metadata_snapshot()?.kind == InodeKind::Directory {
        Ok(())
    } else {
        Err(FsError::NotDirectory)
    }
}
/// 在已有类型化快照上检查访问位；写意图另外检查所属挂载能力。
fn check_access_metadata(
    path: &Path,
    metadata: &InodeMetadata,
    uid: u32,
    gid: u32,
    mask: u16,
) -> Result<(), FsError> {
    if mask & 2 != 0 {
        path.check_writable()?;
    }
    check_inode_mode_access(metadata, uid, gid, mask)
}

/// 使用canonical owner/group/other位完成DAC，不获取元数据锁。
fn check_inode_mode_access(
    metadata: &InodeMetadata,
    uid: u32,
    gid: u32,
    mask: u16,
) -> Result<(), FsError> {
    if mask & !7 != 0 {
        return Err(FsError::InvalidOperation);
    }
    if uid == 0 {
        if mask & 1 != 0 && metadata.kind != InodeKind::Directory && metadata.mode & 0o111 == 0 {
            return Err(FsError::Permission);
        }
        return Ok(());
    }
    let shift = if uid == metadata.uid {
        6
    } else if gid == metadata.gid {
        3
    } else {
        0
    };
    if (metadata.mode >> shift) & mask == mask {
        Ok(())
    } else {
        Err(FsError::Permission)
    }
}

/// 父目录仍连接且当前凭据同时拥有写和搜索权限时才能新建名字。
fn check_parent_write(
    path: &Path,
    metadata: &InodeMetadata,
    uid: u32,
    gid: u32,
) -> Result<(), FsError> {
    path.check_writable()?;
    if metadata.links == 0 {
        return Err(FsError::NotFound);
    }
    if metadata.kind != InodeKind::Directory {
        return Err(FsError::NotDirectory);
    }
    check_inode_mode_access(metadata, uid, gid, 3)
}

/// 对已查到名字的父目录重查DAC，不重复读取或检查Mount状态。
fn check_parent_permissions(metadata: &InodeMetadata, uid: u32, gid: u32) -> Result<(), FsError> {
    if metadata.links == 0 {
        return Err(FsError::NotFound);
    }
    if metadata.kind != InodeKind::Directory {
        return Err(FsError::NotDirectory);
    }
    check_inode_mode_access(metadata, uid, gid, 3)
}

/// 粘滞目录只允许目录属主、目标属主或root删除目标名字。
fn check_sticky_directory(
    parent: &InodeMetadata,
    target: &InodeMetadata,
    uid: u32,
) -> Result<(), FsError> {
    if parent.mode & 0o1000 != 0 && uid != 0 && uid != parent.uid && uid != target.uid {
        return Err(FsError::NotPermitted);
    }
    Ok(())
}

/// 沿真实名字和挂载父关系判断目录，阻止rename把目录放入自身后代。
fn is_path_within(ancestor: &Path, path: &Path) -> bool {
    let mut current = path.clone();
    loop {
        if current.ptr_eq(ancestor) {
            return true;
        }
        let Some(parent) = current.topology_parent() else {
            return false;
        };
        current = parent;
    }
}

/// 一次性拥有原始末组件，后续 Path 和 dentry 只克隆 Arc。
fn try_raw_name(name: &[u8]) -> Result<Arc<Vec<u8>>, FsError> {
    if 255 < name.len() {
        return Err(FsError::NameTooLong);
    }
    if name.is_empty() || name.contains(&0) || name.contains(&b'/') {
        return Err(FsError::InvalidName);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(name.len())
        .map_err(|_| FsError::NoMemory)?;
    bytes.extend_from_slice(name);
    Arc::try_new(bytes).map_err(|_| FsError::NoMemory)
}

/// 短锁内按私有挂载实例检查路径属于当前视图，不能仅按后端卷号判断。
fn contains_mount(mounts: &[Arc<Mount>], mount: &Arc<Mount>) -> bool {
    mounts.iter().any(|entry| Arc::ptr_eq(entry, mount))
}

/// 借用表内登记找到一个覆盖关系，不在短锁中构造或释放路径。
fn find_mount<'a>(
    mounts: &'a [Arc<Mount>],
    parent: &Arc<Mount>,
    dentry: &DentryRef,
) -> Option<&'a Arc<Mount>> {
    mounts.iter().find(|mount| {
        mount.covered.as_ref().is_some_and(|covered| {
            Arc::ptr_eq(&covered.mount, parent) && covered.dentry.ptr_eq(dentry)
        })
    })
}
