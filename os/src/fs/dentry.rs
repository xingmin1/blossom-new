//! 原始组件按父 dentry 身份缓存；名字加载只发表一次，目录 I/O 不持索引锁。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::marker::PhantomPinned;
use core::pin::{pin, Pin};
#[cfg(feature = "ktest")]
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::Once;

use crate::fs::{FsError, InodeCache, InodeKind, InodeRef};
use crate::sched;
use crate::sync::SpinLock;
use crate::wait::{WaitNode, WaitQueue, Waiter};

#[cfg(feature = "ktest")]
type LookupHook = Arc<dyn Fn(&DentryRef) + Send + Sync>;

/// 有限索引强持名字对象，对象只强持父与 inode，不反持缓存域。
/// ponytail: 固定预算内线性查找，预算增大且查找成为瓶颈时改为有限散列表。
pub struct DentryCache {
    entries: SpinLock<Vec<DentryRef>>,
    capacity: usize,
    #[cfg(feature = "ktest")]
    lookup_hook: SpinLock<Option<LookupHook>>,
    #[cfg(feature = "ktest")]
    loads: AtomicUsize,
}

/// 私有 Arc 固定对象和内嵌等待源；共享拥有者不能移出或修改原对象。
#[derive(Clone)]
pub struct DentryRef(Arc<Dentry>);

struct Dentry {
    parent: Option<DentryRef>,
    name: Vec<u8>,
    // 尚无结果时处于 InLookup；完成后只保留正绑定、负绑定或首次具体错误。
    binding: Once<Result<Binding, FsError>>,
    waiters: WaitQueue,
    _pinned: PhantomPinned,
}

enum Binding {
    Positive(InodeRef),
    Negative,
}

/// 胜出者在发表 InLookup 前承担责任，退出必须结束原对象并撤自己的槽。
struct LoadGuard<'a> {
    cache: &'a DentryCache,
    dentry: &'a DentryRef,
    active: bool,
}

impl DentryCache {
    /// 锁外预留全部有限槽位，后续发表不在索引锁内分配。
    pub fn try_new(capacity: usize) -> Result<Self, FsError> {
        if capacity == 0 {
            return Err(FsError::InvalidOperation);
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(capacity)
            .map_err(|_| FsError::NoMemory)?;
        Ok(Self {
            entries: SpinLock::new(entries),
            capacity,
            #[cfg(feature = "ktest")]
            lookup_hook: SpinLock::new(None),
            #[cfg(feature = "ktest")]
            loads: AtomicUsize::new(0),
        })
    }

    /// 普通可切换 Task 查单组件；命中不分配，竞争者在原对象上等待首次结果。
    pub fn lookup(
        &self,
        inodes: &InodeCache,
        parent: &DentryRef,
        name: &[u8],
    ) -> Result<DentryRef, FsError> {
        sched::assert_switchable();
        if parent.inode()?.metadata_snapshot()?.kind != InodeKind::Directory {
            return Err(FsError::NotDirectory);
        }
        validate_name(name)?;
        let (existing, full) = {
            let entries = self.entries.lock();
            (find(&entries, parent, name), self.capacity <= entries.len())
        };
        if let Some(dentry) = existing {
            dentry.inode()?;
            return Ok(dentry);
        }
        // 静止正绑定仍强持 inode；inode 预算满时须先释放名字引用，才能由 iget 回收 inode。
        if full || inodes.is_full() {
            self.prune_unused();
            // 回收期间同 key 可能已发表；预算满也必须先消费现有对象。
            let entries = self.entries.lock();
            let existing = find(&entries, parent, name);
            let full = self.capacity <= entries.len();
            drop(entries);
            if let Some(dentry) = existing {
                dentry.inode()?;
                return Ok(dentry);
            }
            if full {
                return Err(FsError::NoMemory);
            }
        }
        let candidate = DentryRef::try_child(parent, name)?;
        let mut loader = LoadGuard {
            cache: self,
            dentry: &candidate,
            active: false,
        };
        let existing = match self.publish(&candidate, &mut loader) {
            Err(FsError::NoMemory) => {
                // 候选准备期间其他 key 可以占满预算；只有限回收一次并再次双查。
                self.prune_unused();
                self.publish(&candidate, &mut loader)?
            }
            result => result?,
        };
        if let Some(dentry) = existing {
            drop(loader);
            drop(candidate);
            dentry.inode()?;
            return Ok(dentry);
        }
        #[cfg(feature = "ktest")]
        {
            self.loads.fetch_add(1, Ordering::Relaxed);
            let hook = self.lookup_hook.lock().clone();
            if let Some(hook) = hook {
                hook(&candidate);
            }
        }
        let result = parent.inode()?.lookup(inodes, name);
        loader.complete(result);
        drop(loader);
        candidate.inode()?;
        Ok(candidate)
    }

    /// 有限摘除 cache-only 的静止正负绑定，锁外释放后重扫使静止子到父逐层收尽。
    pub fn prune_unused(&self) -> usize {
        let budget = self.len();
        let mut removed = 0;
        let mut position = budget;
        // 摘除会重排索引，且子析构可让父变为 cache-only；每次成功后重扫。
        // 摘除数不超过入口槽数，并发新发表不会把本次回收变成无界循环。
        while 0 < position && removed < budget {
            position -= 1;
            let victim = {
                let mut entries = self.entries.lock();
                let unused = entries.get(position).is_some_and(|dentry| {
                    Arc::strong_count(&dentry.0) == 1
                        && matches!(dentry.0.binding.get(), Some(Ok(_)))
                });
                if unused {
                    Some(entries.swap_remove(position))
                } else {
                    None
                }
            };
            if victim.is_some() {
                removed += 1;
                position = self.len();
            }
            drop(victim);
        }
        removed
    }

    /// 返回有限索引的占用槽数，未完成名字也占用一个槽。
    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    /// 观察缓存域当前是否没有已发表名字。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 同短锁内双查身份与预算，加载责任先于 InLookup 的索引引用发表。
    fn publish(
        &self,
        candidate: &DentryRef,
        loader: &mut LoadGuard<'_>,
    ) -> Result<Option<DentryRef>, FsError> {
        let mut entries = self.entries.lock();
        let parent = candidate.parent().expect("named dentry has no parent");
        if let Some(dentry) = find(&entries, parent, candidate.name()) {
            return Ok(Some(dentry));
        }
        if self.capacity <= entries.len() {
            return Err(FsError::NoMemory);
        }
        loader.active = true;
        entries.push(candidate.clone());
        Ok(None)
    }

    /// 只摘失败的原实例，旧等待者仍持原结果，后续同 key 的新实例不受影响。
    fn remove_failed(&self, dentry: &DentryRef) {
        let victim = {
            let mut entries = self.entries.lock();
            entries
                .iter()
                .position(|entry| entry.ptr_eq(dentry))
                .map(|position| entries.swap_remove(position))
        };
        drop(victim);
    }

    /// 在 InLookup 发表后、真实目录查找前执行锁外停点，替换后的旧回调锁外析构。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_set_lookup_hook(&self, hook: Option<LookupHook>) {
        let old = core::mem::replace(&mut *self.lookup_hook.lock(), hook);
        drop(old);
    }

    /// 返回胜出并取得名字加载责任的次数，不计命中与未发表的竞争失败候选。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_load_count(&self) -> usize {
        self.loads.load(Ordering::Relaxed)
    }

    /// 借用真实索引对象，读取它自己的等待队列成员数。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_waiting(&self, parent: &DentryRef, name: &[u8]) -> usize {
        let dentry = find(&self.entries.lock(), parent, name);
        dentry.map_or(0, |dentry| dentry.0.waiters.test_members())
    }
}

impl DentryRef {
    /// 以真实正目录建立无父、空名字且已完成的挂载根，不参与名字索引。
    pub fn try_root(inode: InodeRef) -> Result<Self, FsError> {
        if inode.metadata_snapshot()?.kind != InodeKind::Directory {
            return Err(FsError::NotDirectory);
        }
        let dentry = Arc::try_new(Dentry {
            parent: None,
            name: Vec::new(),
            binding: Once::new(),
            waiters: WaitQueue::new(),
            _pinned: PhantomPinned,
        })
        .map_err(|_| FsError::NoMemory)?;
        dentry.binding.call_once(|| Ok(Binding::Positive(inode)));
        Ok(Self(dentry))
    }

    /// 锁外准备名字和稳定 Arc；分配失败不会占槽，也没有已登记等待者。
    fn try_child(parent: &Self, name: &[u8]) -> Result<Self, FsError> {
        let mut owned_name = Vec::new();
        owned_name
            .try_reserve_exact(name.len())
            .map_err(|_| FsError::NoMemory)?;
        owned_name.extend_from_slice(name);
        let dentry = Arc::try_new(Dentry {
            parent: Some(parent.clone()),
            name: owned_name,
            binding: Once::new(),
            waiters: WaitQueue::new(),
            _pinned: PhantomPinned,
        })
        .map_err(|_| FsError::NoMemory)?;
        Ok(Self(dentry))
    }

    /// 活子对象强持父，根没有父；借用不制造额外身份或拥有环。
    pub fn parent(&self) -> Option<&DentryRef> {
        self.0.parent.as_ref()
    }

    /// 借用盘上单组件的原始字节，根的名字为空。
    pub fn name(&self) -> &[u8] {
        &self.0.name
    }

    /// 比较名字对象实例身份，硬链接的同 inode 不代表同 dentry。
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// 锁外等待首次结果并借用唯一 inode；负绑定与真实失败保留各自错误。
    pub fn inode(&self) -> Result<&InodeRef, FsError> {
        sched::assert_switchable();
        let dentry = self.0.pinned().get_ref();
        loop {
            if let Some(result) = dentry.binding.get() {
                return binding_inode(result);
            }
            let waiter = Waiter::new();
            let mut node = pin!(WaitNode::new(waiter.waker()));
            // SAFETY: 私有 Arc 在本借用期间保活固定 dentry 和内嵌等待源；pin! 固定栈节点，
            // registration 不被遗忘，离开本轮先撤链，再释放节点、waiter 和源借用。
            let _registration = unsafe { dentry.waiters.register(node.as_mut()) };
            if let Some(result) = dentry.binding.get() {
                return binding_inode(result);
            }
            waiter.park();
        }
    }
}

impl Dentry {
    /// 私有稳定拥有类型只给共享借用，固定等待源不能从原 Arc 分配中移出。
    fn pinned(&self) -> Pin<&Self> {
        // SAFETY: 对象仅经私有构造置于 Arc 后发表；本模块不 unwrap、替换或移动对象，
        // DentryRef 不暴露 Arc 或可变借用，本共享借用保证原分配存活。
        unsafe { Pin::new_unchecked(self) }
    }
}

impl LoadGuard<'_> {
    /// 真实查找结束后只将 NotFound 变为负绑定，其他错误解除占位并唤醒等待者。
    fn complete(&mut self, result: Result<InodeRef, FsError>) {
        let binding = match result {
            Ok(inode) => Ok(Binding::Positive(inode)),
            Err(FsError::NotFound) => Ok(Binding::Negative),
            Err(error) => Err(error),
        };
        self.publish(binding);
        self.active = false;
    }

    /// 结果只发表一次，失败摘原槽后锁外通知，旧等待者不改查新的加载实例。
    fn publish(&self, binding: Result<Binding, FsError>) {
        let result = self.dentry.0.binding.call_once(|| binding);
        if result.is_err() {
            self.cache.remove_failed(self.dentry);
        }
        self.dentry.0.waiters.wake_all();
    }
}

impl Drop for LoadGuard<'_> {
    /// 已发表但未完成的加载者退出时结束原对象，避免遗留永久 InLookup。
    fn drop(&mut self) {
        if self.active {
            self.publish(Err(FsError::LoadAborted));
        }
    }
}

/// 索引锁内只克隆现有对象，key 使用父实例身份与原始组件字节。
fn find(entries: &[DentryRef], parent: &DentryRef, name: &[u8]) -> Option<DentryRef> {
    entries
        .iter()
        .find(|dentry| {
            dentry.name() == name && dentry.parent().is_some_and(|entry| entry.ptr_eq(parent))
        })
        .cloned()
}

/// 单组件沿盘上名字长度约束验证，点组件的路径含义由逐组件遍历处理。
fn validate_name(name: &[u8]) -> Result<(), FsError> {
    if name.is_empty() || 255 < name.len() || name.contains(&0) || name.contains(&b'/') {
        return Err(FsError::InvalidName);
    }
    Ok(())
}

/// 从已完成的一次绑定借用 inode，不复制对象或将失败误当负绑定。
fn binding_inode(binding: &Result<Binding, FsError>) -> Result<&InodeRef, FsError> {
    match binding {
        Ok(Binding::Positive(inode)) => Ok(inode),
        Ok(Binding::Negative) => Err(FsError::NotFound),
        Err(error) => Err(*error),
    }
}
