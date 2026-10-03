//! 单 Normal zone 的连续页块自由链，元数据仅在 zone 锁域内改变。

#[cfg(feature = "boot-tests")]
use core::mem::MaybeUninit;
use spin::Once;

#[cfg(feature = "boot-tests")]
use crate::mm::page::PageDesc;
use crate::mm::page::{BuddyLink, PageKind, PageMap};
use crate::sync::SpinLock;

pub(crate) const MAX_ORDER: usize = 10;
pub(crate) const BATCH_CAPACITY: usize = 8;
const NONE: usize = usize::MAX;

/// 唯一连续块释放权；移动不会复制归还责任，不自动归还。
pub(crate) struct Block {
    head: usize,
    order: usize,
}

impl Block {
    /// 返回连续块首 PFN，不授予额外释放权。
    pub(crate) fn head_pfn(&self) -> usize {
        self.head
    }

    /// 返回分配时的阶，归还必须保留此值。
    pub(crate) fn order(&self) -> usize {
        self.order
    }

    /// 恢复已从拥有者或 PCP 链转移的真实整体块唯一归还权，不改变 zone 元数据。
    ///
    /// # Safety
    /// head 必须是本 buddy 原分配且仍 live 的整体块首 PFN，order 必须是原阶；
    /// 原拥有凭证已消费或页刚从 PCP 链摘离，全部访问已结束，块从未归还，
    /// 不在任何 buddy、PCP 或 slab 自由链中，也无另一份有效归还凭证。
    pub(crate) unsafe fn from_raw(head: usize, order: usize) -> Self {
        Self { head, order }
    }
}

struct Zone<'a> {
    memory: &'a PageMap<'a>,
    heads: [usize; MAX_ORDER + 1],
    free: usize,
}

impl<'a> Zone<'a> {
    /// 建立尚未接管自由范围的唯一锁域，之后仅通过其锁 guard 调用方法。
    ///
    /// # Safety
    /// memory 的所有描述符不得同时归属其他 zone；其 buddy 字段仅由此锁域访问。
    unsafe fn empty(memory: &'a PageMap<'a>) -> Self {
        Self {
            memory,
            heads: [NONE; MAX_ORDER + 1],
            free: 0,
        }
    }

    /// 持有所属 zone 锁时读取自由头状态，每个 PFN 独立通过 PageMap 定位。
    fn link(&self, pfn: usize) -> BuddyLink {
        let desc = self.memory.get(pfn).expect("buddy PFN has no descriptor");
        // SAFETY: Zone 唯一拥有该描述符的锁域，调用者持有此 zone guard；
        // 仅复制已初始化字段，不产生可变引用，也不访问其他描述符数组。
        unsafe { desc.buddy.get().read() }
    }

    /// 持有所属 zone 锁时替换一个自由头的链状态。
    fn set_link(&mut self, pfn: usize, link: BuddyLink) {
        let desc = self.memory.get(pfn).expect("buddy PFN has no descriptor");
        // SAFETY: Zone 唯一拥有该描述符的锁域，当前 guard 排除其他字段访问；
        // UnsafeCell 内目标已初始化且永不释放，不与不可变 kind 字段重叠。
        unsafe { desc.buddy.get().write(link) };
    }

    /// 将真实自由块加入对应阶链，并在同一临界区增加自由页数。
    fn insert(&mut self, head: usize, order: usize) {
        let next = self.heads[order];
        if next != NONE {
            let mut link = self.link(next);
            link.previous = head;
            self.set_link(next, link);
        }
        self.set_link(
            head,
            BuddyLink {
                order,
                previous: NONE,
                next,
            },
        );
        self.heads[order] = head;
        self.free += 1 << order;
    }

    /// 以头内双向链 O(1) 摘块，同时清除自由头身份和相应自由页数。
    fn remove(&mut self, head: usize, order: usize) {
        let link = self.link(head);
        if link.previous == NONE {
            self.heads[order] = link.next;
        } else {
            let mut previous = self.link(link.previous);
            previous.next = link.next;
            self.set_link(link.previous, previous);
        }
        if link.next != NONE {
            let mut next = self.link(link.next);
            next.previous = link.previous;
            self.set_link(link.next, next);
        }
        self.set_link(
            head,
            BuddyLink {
                order: NONE,
                previous: NONE,
                next: NONE,
            },
        );
        self.free -= 1 << order;
    }

    /// 仅接管最终 free_ranges，按绝对 PFN 对齐拆成不越界的最大阶块。
    fn initialize(&mut self) {
        let memory = self.memory;
        for range in memory.free_ranges() {
            let mut head = range.start;
            while head < range.end {
                let mut order = (head.trailing_zeros() as usize).min(MAX_ORDER);
                while range.end - head < (1 << order) {
                    order -= 1;
                }
                self.insert(head, order);
                head += 1 << order;
            }
        }
    }

    /// 摘下最小适合阶并逐次拆分；失败不改变链，也不拼接零散页。
    fn allocate(&mut self, order: usize) -> Option<Block> {
        if MAX_ORDER < order {
            return None;
        }
        let mut found = order;
        while found <= MAX_ORDER && self.heads[found] == NONE {
            found += 1;
        }
        if MAX_ORDER < found {
            return None;
        }
        let head = self.heads[found];
        self.remove(head, found);
        while order < found {
            found -= 1;
            self.insert(head + (1 << found), found);
        }
        Some(Block { head, order })
    }

    /// 归还原连续块，只与同一描述符锁域中的同阶真实自由头合并。
    ///
    /// # Safety
    /// 调用者拥有本 zone 曾分配的完整 head/order 块，已结束访问且只归还一次；
    /// 该块当前不在 buddy、PCP 或 slab 的任何自由链中。
    unsafe fn release(&mut self, mut head: usize, mut order: usize) {
        while order < MAX_ORDER {
            let partner = head ^ (1 << order);
            let Some(desc) = self.memory.get(partner) else {
                break;
            };
            if desc.kind() != PageKind::Available || self.link(partner).order != order {
                break;
            }
            self.remove(partner, order);
            head = head.min(partner);
            order += 1;
        }
        self.insert(head, order);
    }
}

static ZONE: Once<SpinLock<Zone<'static>>> = Once::new();

// 独立测试元数据仅在借用期间由此锁独占，初始化不构造或复制巨型栈数组。
// 该 arena 不发布到全局 PageMap、不提供物理页 backing，也不参加生产页分配。
#[cfg(feature = "boot-tests")]
static TEST_DESCRIPTORS: SpinLock<[MaybeUninit<PageDesc>; 1 << MAX_ORDER]> =
    SpinLock::new([const { MaybeUninit::uninit() }; 1 << MAX_ORDER]);

/// 在 BSP 独占启动期一次性接管永久 PageMap 的最终自由页。
///
/// # Safety
/// memory 与其描述符必须永久发布；只能调用一次，IRQ 关闭、无并发 alloc/free，
/// free_ranges 尚未移交任何拥有者，描述符 buddy 字段不归其他 zone 锁域管理。
pub(crate) unsafe fn initialize(memory: &'static PageMap<'static>) {
    assert!(ZONE.get().is_none(), "buddy initialized twice");
    ZONE.call_once(|| {
        // SAFETY: 调用者保证描述符永久且此前未被任何 zone 接管。
        let zone = SpinLock::new(unsafe { Zone::empty(memory) });
        zone.lock().initialize();
        zone
    });
}

/// 不睡眠地申请真实连续块；返回前释放 zone 锁，不在锁内清页。
pub(crate) fn allocate(order: usize) -> Option<Block> {
    ZONE.get()
        .expect("buddy is not initialized")
        .lock()
        .allocate(order)
}

/// 在一份 zone guard 下逐项摘取独立零阶块，不足 count 时保留已成功取得的块。
/// output 必须全为空；固定表仅承担移动型 Block 的临时归还责任，不要求页相邻。
pub(crate) fn allocate_batch(output: &mut [Option<Block>; BATCH_CAPACITY], count: usize) -> usize {
    assert!(
        count <= BATCH_CAPACITY,
        "buddy batch exceeds fixed capacity"
    );
    assert!(
        output.iter().all(Option::is_none),
        "buddy batch output is occupied"
    );
    let mut zone = ZONE.get().expect("buddy is not initialized").lock();
    let mut allocated = 0;
    for slot in &mut output[..count] {
        let Some(block) = zone.allocate(0) else {
            break;
        };
        *slot = Some(block);
        allocated += 1;
    }
    allocated
}

/// 在一份 zone guard 下消耗固定表中所有零阶块并归还，空槽不产生释放动作。
///
/// # Safety
/// 所有非空槽持有本 buddy 分配的不同零阶页唯一归还权，页访问已结束；
/// 页已从 PCP 链摘离，当前不属于任何 buddy、PCP 或 slab 自由链。
pub(crate) unsafe fn release_batch(blocks: &mut [Option<Block>; BATCH_CAPACITY]) {
    assert!(
        blocks.iter().flatten().all(|block| block.order == 0),
        "buddy batch contains a nonzero order"
    );
    let mut zone = ZONE.get().expect("buddy is not initialized").lock();
    for slot in blocks {
        if let Some(block) = slot.take() {
            // SAFETY: 调用者保证每份 Block 对应已摘离且访问结束的唯一零阶页；
            // take 消耗此槽归还权，本批全程持有这些页所属的同一 zone guard。
            unsafe { zone.release(block.head, 0) };
        }
    }
}

/// 将整体释放权归还唯一 zone，按原阶恢复自由链。
///
/// # Safety
/// 调用者拥有 allocate 所返同一块全部释放权和原 order；必须已结束所有访问，
/// 未归还过，且块当前不在任何 buddy、PCP 或 slab 自由链中。
pub(crate) unsafe fn release(head_pfn: usize, order: usize) {
    // SAFETY: 调用者承担完整块的唯一归还责任；固定 ZONE 即原分配锁域。
    unsafe {
        ZONE.get()
            .expect("buddy is not initialized")
            .lock()
            .release(head_pfn, order)
    };
}

/// 在 zone 锁内取得 buddy 自由页快照，不包括其他缓存保留页。
pub(crate) fn free_pages() -> usize {
    ZONE.get().expect("buddy is not initialized").lock().free
}

/// 使用生产 Zone 方法验证全部阶、非顺序归还、碎片失败及受控池真实穷尽。
#[cfg(feature = "boot-tests")]
pub(crate) fn test_orders_and_fragmentation() -> bool {
    test_orders() && test_fragmentation()
}

/// 消耗独立受控池唯一块句柄，避免测试复制裸 PFN 的释放责任。
#[cfg(feature = "boot-tests")]
fn return_block(zone: &mut Zone<'_>, block: Block) {
    // SAFETY: 测试只传入该 zone 刚分配且未访问物理内容的唯一 Block，消耗后不再归还。
    unsafe { zone.release(block.head, block.order) };
}

/// 独占静态 1024 槽 arena，覆盖 order0–10、混合阶不重叠与最大块恢复；zone 借用结束前不复用槽。
#[cfg(feature = "boot-tests")]
fn test_orders() -> bool {
    use crate::mm::bootmem::PageRange;

    let mut storage = TEST_DESCRIPTORS.lock();
    for slot in storage.iter_mut() {
        slot.write(PageDesc::new(PageKind::Available));
    }
    // SAFETY: 静态 arena 在 storage guard 存活期间独占，1024 槽已逐项写入有效值；
    // 切片仅覆盖本数组，全部 map/zone 借用先于 guard 销毁；测试从不设置 slab 拥有状态。
    let descriptors =
        unsafe { core::slice::from_raw_parts(storage.as_ptr().cast(), storage.len()) };
    let sections = [Some(descriptors)];
    let ranges = [PageRange {
        start: 0,
        end: 1 << MAX_ORDER,
    }];
    let memory = PageMap::test_map(&sections, &ranges);
    // SAFETY: 此局部 PageMap 仅交给一把测试 zone 锁，描述符与全局 PageMap 不重叠。
    let lock = SpinLock::new(unsafe { Zone::empty(&memory) });
    let mut zone = lock.lock();
    zone.initialize();
    if zone.allocate(MAX_ORDER + 1).is_some() || zone.free != 1 << MAX_ORDER {
        return false;
    }
    let Some(maximum) = zone.allocate(MAX_ORDER) else {
        return false;
    };
    if maximum.head_pfn() != 0 || maximum.order() != MAX_ORDER || zone.free != 0 {
        return false;
    }
    return_block(&mut zone, maximum);

    let mut blocks: [Option<Block>; MAX_ORDER + 1] = core::array::from_fn(|_| None);
    for (index, slot) in blocks.iter_mut().enumerate() {
        let order = if index == MAX_ORDER { 0 } else { index };
        let before = zone.free;
        let Some(block) = zone.allocate(order) else {
            return false;
        };
        if block.order() != order
            || block.head_pfn() & ((1 << order) - 1) != 0
            || storage.len() < block.head_pfn() + (1 << order)
            || zone.free != before - (1 << order)
        {
            return false;
        }
        *slot = Some(block);
    }
    for (index, first) in blocks.iter().enumerate() {
        let first = first.as_ref().unwrap();
        for second in blocks[index + 1..].iter().flatten() {
            if first.head + (1 << first.order) <= second.head
                || second.head + (1 << second.order) <= first.head
            {
                continue;
            }
            return false;
        }
    }
    if zone.free != 0 || zone.allocate(0).is_some() {
        return false;
    }
    for index in [9, 0, 7, 2, 10, 4, 1, 8, 3, 6, 5] {
        return_block(&mut zone, blocks[index].take().unwrap());
    }
    if zone.free != 1 << MAX_ORDER {
        return false;
    }
    let Some(restored) = zone.allocate(MAX_ORDER) else {
        return false;
    };
    if restored.head != 0 || restored.order != MAX_ORDER || zone.free != 0 {
        return false;
    }
    return_block(&mut zone, restored);
    zone.free == 1 << MAX_ORDER
}

/// 以保留槽与空洞切断连续块，在总量足够时失败并逐页耗尽、归还恢复。
#[cfg(feature = "boot-tests")]
fn test_fragmentation() -> bool {
    use crate::mm::bootmem::PageRange;

    let descriptors: [PageDesc; 16] = core::array::from_fn(|pfn| {
        PageDesc::new(match pfn {
            3 | 11 => PageKind::Reserved,
            7 | 15 => PageKind::Absent,
            _ => PageKind::Available,
        })
    });
    // 中间整 section 无描述符；高 section 使用另一独立数组，伙伴仍按绝对 PFN 定位。
    let high = [const { PageDesc::new(PageKind::Available) }; 2];
    let sections = [Some(descriptors.as_slice()), None, Some(high.as_slice())];
    let ranges = [
        PageRange { start: 0, end: 3 },
        PageRange { start: 4, end: 7 },
        PageRange { start: 8, end: 11 },
        PageRange { start: 12, end: 15 },
        PageRange {
            start: 65536,
            end: 65538,
        },
    ];
    let memory = PageMap::test_map(&sections, &ranges);
    // SAFETY: 两个数组均为本测试独立元数据，仅此 zone 访问其 buddy 字段。
    let lock = SpinLock::new(unsafe { Zone::empty(&memory) });
    let mut zone = lock.lock();
    zone.initialize();
    if zone.free != 14 {
        return false;
    }
    if zone.allocate(3).is_some() || zone.free != 14 {
        return false;
    }
    let mut blocks: [Option<Block>; 14] = core::array::from_fn(|_| None);
    for index in 0..blocks.len() {
        let Some(block) = zone.allocate(0) else {
            return false;
        };
        if !ranges
            .iter()
            .any(|range| range.start <= block.head && block.head < range.end)
            || blocks[..index]
                .iter()
                .flatten()
                .any(|previous| previous.head == block.head)
        {
            return false;
        }
        blocks[index] = Some(block);
    }
    if zone.free != 0 || zone.allocate(0).is_some() {
        return false;
    }
    // 互质步长覆盖所有句柄，归还顺序与申请顺序不同。
    for step in 0..blocks.len() {
        let index = (step * 5) % blocks.len();
        return_block(&mut zone, blocks[index].take().unwrap());
    }
    if zone.free != 14 {
        return false;
    }
    if zone.allocate(2).is_some() || zone.free != 14 {
        return false;
    }
    let mut pairs: [Option<Block>; 5] = core::array::from_fn(|_| None);
    for slot in &mut pairs {
        let Some(block) = zone.allocate(1) else {
            return false;
        };
        *slot = Some(block);
    }
    if zone.allocate(1).is_some() || zone.free != 4 {
        return false;
    }
    for slot in &mut pairs {
        return_block(&mut zone, slot.take().unwrap());
    }
    zone.free == 14
}
