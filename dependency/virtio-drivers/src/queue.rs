//! 外部分配的连贯内存上的 split virtqueue；只消费已映射的设备地址。

#![deny(unsafe_op_in_unsafe_fn)]

use crate::{BufferDirection, DmaAddr, DmaRegion, Error, Hal, MappedSegment, Result, PAGE_SIZE};
use alloc::vec::Vec;
use core::{marker::PhantomData, ptr};

const DESC_BYTES: usize = 16;
const NEXT: u16 = 1;
const WRITE: u16 = 2;
const NO_INTERRUPT: u16 = 1;
const NO_NOTIFY: u16 = 1;

/// 按真实队列深度计算的单一连续 split ring 布局。
#[derive(Copy, Clone, Debug)]
pub struct RingLayout {
    depth: u16,
    avail_offset: usize,
    used_offset: usize,
    byte_len: usize,
    alignment: usize,
}

impl RingLayout {
    /// 接受 4 到 128 的二次幂深度，保留协议的两个 event 尾字段空间。
    pub fn new(depth: u16, legacy: bool) -> Result<Self> {
        if !(4..=128).contains(&depth) || !depth.is_power_of_two() {
            return Err(Error::InvalidParam);
        }
        let count = usize::from(depth);
        let avail_offset = DESC_BYTES * count;
        let avail_end = avail_offset + 6 + 2 * count;
        let used_alignment = if legacy { PAGE_SIZE } else { 4 };
        let used_offset = (avail_end + used_alignment - 1) & !(used_alignment - 1);
        Ok(Self {
            depth,
            avail_offset,
            used_offset,
            byte_len: used_offset + 6 + 8 * count,
            alignment: if legacy { PAGE_SIZE } else { DESC_BYTES },
        })
    }

    /// 返回描述符表、avail 和 used 共同使用的真实深度。
    pub const fn depth(self) -> u16 {
        self.depth
    }

    /// 返回控制内存必须实际映射的最小字节数。
    pub const fn byte_len(self) -> usize {
        self.byte_len
    }

    /// 返回 driver area 相对 descriptor 表的字节偏移。
    pub const fn avail_offset(self) -> usize {
        self.avail_offset
    }

    /// 返回 device area 的字节偏移，legacy 已按页对齐。
    pub const fn used_offset(self) -> usize {
        self.used_offset
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct Descriptor {
    address: u64,
    length: u32,
    flags: u16,
    next: u16,
}

#[derive(Copy, Clone, Debug)]
struct Shadow {
    next: u16,
    // 仅活动链的头保存非零长度，链内节点不能作为 used head。
    chain_len: u16,
}

/// 一条真实 used 元素；head 用于调用者的唯一请求拥有表。
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct UsedElement {
    /// 已提交 descriptor 链的真实头。
    pub head: u16,
    /// 设备报告的已写字节数，协议层仍须验证其请求语义。
    pub len: u32,
}

/// 持有可信 shadow，不拥有 ring 或 payload 的页资源。
#[derive(Debug)]
pub struct VirtQueue<H: Hal> {
    layout: RingLayout,
    region: DmaRegion,
    shadow: Vec<Shadow>,
    free_head: u16,
    num_free: u16,
    inflight: u16,
    avail_idx: u16,
    last_used_idx: u16,
    _hal: PhantomData<H>,
}

// SAFETY: 构造与 add 的调用者保证页拥有者跨线程保活；移动队列不访问 DMA 内存，
// 后续操作仍通过独占借用串行化，设备只访问外部连贯映射。
unsafe impl<H: Hal> Send for VirtQueue<H> {}

impl<H: Hal> VirtQueue<H> {
    /// 在普通任务、队列锁外准备 fallible shadow，不向设备发表任何地址。
    ///
    /// # Safety
    ///
    /// region 必须是同一块可读写连贯内存的真实 CPU/设备映射，当前没有设备访问。
    /// 实际页和 CPU 映射必须覆盖队列及其后续持有者的全部 CPU 访问，通常保活至队列销毁。
    /// 归还前还须确认设备已停止访问；仅确认 reset 不结束仍可调用的安全 CPU 访问。
    /// 不得并发访问 driver-owned 字段或建立覆盖 DMA 可写字段的 Rust 引用。
    pub unsafe fn new(layout: RingLayout, region: DmaRegion) -> Result<Self> {
        let cpu = region.cpu.as_ptr() as usize;
        if region.len < layout.byte_len
            || cpu % layout.alignment != 0
            || region.device.value() % layout.alignment as u64 != 0
            || cpu.checked_add(layout.byte_len).is_none()
            || region.device.checked_add(layout.byte_len - 1).is_none()
        {
            return Err(Error::InvalidParam);
        }
        let mut shadow = Vec::new();
        shadow
            .try_reserve_exact(usize::from(layout.depth))
            .map_err(|_| Error::NoMemory)?;
        for index in 0..layout.depth {
            shadow.push(Shadow {
                next: index + 1,
                chain_len: 0,
            });
        }
        // SAFETY: 调用者提供实际可写且无设备访问的映射；布局长度已检查，
        // 所有协议字段仅含整数，零填不构造 Rust 引用。
        unsafe { ptr::write_bytes(region.cpu.as_ptr(), 0, layout.byte_len) };
        Ok(Self {
            layout,
            region,
            shadow,
            free_head: 0,
            num_free: layout.depth,
            inflight: 0,
            avail_idx: 0,
            last_used_idx: 0,
            _hal: PhantomData,
        })
    }

    /// 返回预备好的 descriptor、driver area 和 device area 设备地址。
    pub fn addresses(&self) -> (DmaAddr, DmaAddr, DmaAddr) {
        // 构造已验证整个地址跨度，偏移严格位于跨度内。
        (
            self.region.device,
            self.region
                .device
                .checked_add(self.layout.avail_offset)
                .unwrap(),
            self.region
                .device
                .checked_add(self.layout.used_offset)
                .unwrap(),
        )
    }

    /// 返回设备实际使用的队列深度。
    pub fn depth(&self) -> u16 {
        self.layout.depth
    }

    /// 返回无需映射或分配即可提交的剩余 descriptor 额度。
    pub fn available_descriptors(&self) -> usize {
        usize::from(self.num_free)
    }

    /// 事务性检查所有片段后，直接向 split ring 提交一条已映射链。
    ///
    /// # Safety
    ///
    /// 调用者须保活每个片段的实际映射及拥有者，直到对应 used 被取出或确认 reset。
    /// DriverToDevice 内容在此期间不得修改；DeviceToDriver 内容不得由 CPU 访问。
    /// 地址须真实映射为声明的方向，且不得与 ring 或仍在访问的 payload 非法别名。
    /// 所有队列操作须串行化，提交后调用者须安装 head 到请求拥有者的唯一关系。
    pub unsafe fn add(&mut self, groups: &[&[MappedSegment]]) -> Result<u16> {
        let count = self.validate_segments(groups)?;
        if usize::from(self.num_free) < count {
            return Err(Error::QueueFull);
        }
        let head = self.free_head;
        let mut index = head;
        let mut remaining = count;
        for group in groups {
            for segment in *group {
                let next = self.shadow[usize::from(index)].next;
                remaining -= 1;
                let flags = if segment.direction == BufferDirection::DeviceToDriver {
                    WRITE
                } else {
                    0
                } | if remaining == 0 { 0 } else { NEXT };
                self.write_descriptor(
                    index,
                    Descriptor {
                        address: segment.address.value().to_le(),
                        length: segment.length.to_le(),
                        flags: flags.to_le(),
                        next: if remaining == 0 { 0 } else { next.to_le() },
                    },
                );
                index = next;
            }
        }
        self.free_head = index;
        self.num_free -= count as u16;
        self.inflight += 1;
        self.shadow[usize::from(head)].chain_len = count as u16;
        let slot = usize::from(self.avail_idx & (self.layout.depth - 1));
        self.write_u16(self.layout.avail_offset + 4 + 2 * slot, head);
        H::dma_write_barrier();
        self.avail_idx = self.avail_idx.wrapping_add(1);
        self.write_u16(self.layout.avail_offset + 2, self.avail_idx);
        Ok(head)
    }

    /// 在任何 ring 或 shadow 修改前检查段范围、读写顺序和总额度。
    fn validate_segments(&self, groups: &[&[MappedSegment]]) -> Result<usize> {
        let mut count = 0usize;
        let mut device_writes = false;
        for group in groups {
            count = count.checked_add(group.len()).ok_or(Error::InvalidParam)?;
            for segment in *group {
                if segment.length == 0
                    || segment
                        .address
                        .checked_add(segment.length as usize - 1)
                        .is_none()
                    || (device_writes && segment.direction == BufferDirection::DriverToDevice)
                {
                    return Err(Error::InvalidParam);
                }
                device_writes |= segment.direction == BufferDirection::DeviceToDriver;
            }
        }
        if count == 0 {
            return Err(Error::InvalidParam);
        }
        Ok(count)
    }

    /// 取出真实 used head 并仅依赖可信 shadow 回收链；错误不归还任何资源。
    pub fn pop_used(&mut self) -> Result<Option<UsedElement>> {
        let used_idx = self.read_u16(self.layout.used_offset + 2);
        let pending = used_idx.wrapping_sub(self.last_used_idx);
        if pending == 0 {
            return Ok(None);
        }
        if self.layout.depth < pending || self.inflight < pending {
            return Err(Error::DeviceNeedsReset);
        }
        H::dma_read_barrier();
        let slot = usize::from(self.last_used_idx & (self.layout.depth - 1));
        let offset = self.layout.used_offset + 4 + 8 * slot;
        let id = self.read_u32(offset);
        if u32::from(self.layout.depth) <= id {
            return Err(Error::WrongToken);
        }
        let head = id as u16;
        let chain_len = self.shadow[usize::from(head)].chain_len;
        if chain_len == 0 {
            return Err(Error::WrongToken);
        }
        let len = self.read_u32(offset + 4);
        let mut tail = head;
        for _ in 1..chain_len {
            tail = self.shadow[usize::from(tail)].next;
        }
        self.shadow[usize::from(tail)].next = self.free_head;
        self.shadow[usize::from(head)].chain_len = 0;
        self.free_head = head;
        self.num_free += chain_len;
        self.inflight -= 1;
        self.last_used_idx = self.last_used_idx.wrapping_add(1);
        Ok(Some(UsedElement { head, len }))
    }

    /// 写 flags 模式的中断抑制位；设备仍可能已有已发出的中断。
    pub fn set_interrupts(&mut self, enabled: bool) {
        self.write_u16(
            self.layout.avail_offset,
            if enabled { 0 } else { NO_INTERRUPT },
        );
        H::dma_barrier();
    }

    /// 重装中断后完整排序并复查 used；true 表示仍需处理完成。
    pub fn enable_interrupts_and_recheck(&mut self) -> bool {
        self.set_interrupts(true);
        self.last_used_idx != self.read_u16(self.layout.used_offset + 2)
    }

    /// 在 avail 发表后完整排序，再读取设备 flags 决定是否需要 doorbell。
    pub fn should_notify(&self) -> bool {
        H::dma_barrier();
        self.read_u16(self.layout.used_offset) & NO_NOTIFY == 0
    }

    /// 读取布局内已对齐的协议 16 位字段，保持设备可见的单次访问。
    fn read_u16(&self, offset: usize) -> u16 {
        debug_assert!(offset + 2 <= self.layout.byte_len && offset % 2 == 0);
        // SAFETY: 构造保证整个布局保活且对齐；私有调用点只读取实际布局内的整数，
        // 不建立与设备可写内存重叠的引用。
        u16::from_le(unsafe {
            ptr::read_volatile(self.region.cpu.as_ptr().add(offset).cast::<u16>())
        })
    }

    /// 读取 used 中已对齐的 32 位整数，保留完整 id 的验证空间。
    fn read_u32(&self, offset: usize) -> u32 {
        debug_assert!(offset + 4 <= self.layout.byte_len && offset % 4 == 0);
        // SAFETY: 构造保证整个布局保活且对齐，调用点在读屏障后访问 used 槽内整数。
        u32::from_le(unsafe {
            ptr::read_volatile(self.region.cpu.as_ptr().add(offset).cast::<u32>())
        })
    }

    /// 写布局内 driver-owned 16 位字段，不借用共享 DMA 内存。
    fn write_u16(&mut self, offset: usize, value: u16) {
        debug_assert!(offset + 2 <= self.layout.byte_len && offset % 2 == 0);
        // SAFETY: 构造保证映射保活且对齐，私有调用点只写布局内 driver-owned 字段，
        // 独占队列借用及调用者协议排除其他 CPU 写入。
        unsafe {
            ptr::write_volatile(
                self.region.cpu.as_ptr().add(offset).cast::<u16>(),
                value.to_le(),
            )
        };
    }

    /// 将预先验证的片段写入一个当前由驱动拥有的 descriptor 槽。
    fn write_descriptor(&mut self, index: u16, descriptor: Descriptor) {
        debug_assert!(index < self.layout.depth);
        // SAFETY: index 来自可信空闲链，descriptor 表完整映射且起点按 16 字节对齐，
        // 该槽尚未发表给设备；这里只进行原始整数写入。
        unsafe {
            ptr::write_volatile(
                self.region
                    .cpu
                    .as_ptr()
                    .add(usize::from(index) * DESC_BYTES)
                    .cast::<Descriptor>(),
                descriptor,
            )
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;
    use core::{
        cell::Cell,
        ptr::NonNull,
        sync::atomic::{fence, Ordering},
    };

    std::thread_local! {
        static COMPLETE_AT_BARRIER: Cell<Option<(*mut u16, u16)>> = const { Cell::new(None) };
    }

    #[derive(Debug)]
    struct ControlledHal;

    // SAFETY: 该实现仅用于同线程受控 ring 字节实验，没有真实 DMA 设备；
    // 原子屏障约束实验 CPU 访问，不能作为硬件平台 Hal 实现使用。
    unsafe impl Hal for ControlledHal {
        /// 排序受控实验中的 descriptor 和 avail 发布。
        fn dma_write_barrier() {
            fence(Ordering::SeqCst);
        }

        /// 排序受控实验中的 used 消费。
        fn dma_read_barrier() {
            fence(Ordering::SeqCst);
        }

        /// 在中断重装窗口模拟一次受控设备发布，再排序 CPU 访问。
        fn dma_barrier() {
            COMPLETE_AT_BARRIER.with(|completion| {
                if let Some((index, value)) = completion.take() {
                    // SAFETY: 实验只安装当前 Fixture 内已对齐的 used.idx 指针，
                    // Fixture 存活到本同步调用结束，没有其他 CPU 或硬件访问。
                    unsafe { ptr::write_volatile(index, value.to_le()) };
                }
            });
            fence(Ordering::SeqCst);
        }
    }

    #[repr(C, align(4096))]
    struct RingBytes([u8; 8192]);

    struct Fixture {
        queue: VirtQueue<ControlledHal>,
        _ring: Box<RingBytes>,
        payload: Box<[u8; 512]>,
    }

    impl Fixture {
        /// 保存真实控制内存与 payload，所有设备侧操作均为同线程受控字节写入。
        fn new(depth: u16, legacy: bool) -> Self {
            let layout = RingLayout::new(depth, legacy).unwrap();
            let mut ring = Box::new(RingBytes([0; 8192]));
            let cpu = NonNull::from(&mut ring.0[0]);
            let region = DmaRegion {
                cpu,
                device: DmaAddr::new(cpu.as_ptr() as usize as u64),
                len: ring.0.len(),
            };
            // SAFETY: ring 的 CPU 与实验设备地址指向同一真实内存，页对齐且
            // 无并发访问；Box 保留到 queue 销毁之后，实验不建立共享引用。
            let queue = unsafe { VirtQueue::new(layout, region) }.unwrap();
            Self {
                queue,
                _ring: ring,
                payload: Box::new([0; 512]),
            }
        }

        /// 提供指向实验拥有的真实 payload 字节的只读设备片段。
        fn segment(&self) -> MappedSegment {
            MappedSegment {
                address: DmaAddr::new(self.payload.as_ptr() as usize as u64),
                length: self.payload.len() as u32,
                direction: BufferDirection::DriverToDevice,
            }
        }

        /// 提交实验内保活的只读 payload 链，不模拟协议 echo。
        fn submit(&mut self, count: usize) -> u16 {
            let segments = [self.segment(); 4];
            // SAFETY: payload Box 保活，实验不修改数据，所有同线程操作串行；
            // 多个只读 descriptor 允许引用同一片段，控制字节独立存储。
            unsafe { self.queue.add(&[&segments[..count]]) }.unwrap()
        }

        /// 写入实验设备拥有的单个 used 元素，不隐式改变 used.idx。
        fn used(&mut self, index: u16, id: u32, len: u32) {
            let slot = usize::from(index & (self.queue.depth() - 1));
            let offset = self.queue.layout.used_offset + 4 + slot * 8;
            self.write_u32(offset, id);
            self.write_u32(offset + 4, len);
        }

        /// 从受控设备侧发表完成索引。
        fn publish_used(&mut self, index: u16) {
            self.queue
                .write_u16(self.queue.layout.used_offset + 2, index);
        }

        /// 直接修改真实 ring 的 32 位整数，用于恶意设备输入实验。
        fn write_u32(&mut self, offset: usize, value: u32) {
            assert!(offset + 4 <= self.queue.layout.byte_len && offset % 4 == 0);
            // SAFETY: Fixture 保活且独占 ring，offset 检查完整整数的范围及对齐；
            // 该操作只模拟同线程设备写入，没有 Rust 引用覆盖此内存。
            unsafe {
                ptr::write_volatile(
                    self.queue.region.cpu.as_ptr().add(offset).cast::<u32>(),
                    value.to_le(),
                )
            };
        }
    }

    /// 反序完成仍按真实 head 归还整链，硬件 descriptor.next 不参与回收。
    #[test]
    fn reversed_heads_recycle_trusted_chains() {
        let mut fixture = Fixture::new(8, false);
        let first = fixture.submit(3);
        let second = fixture.submit(2);
        // 设备恶意改写首节点的 flags/next 为全一，可信 shadow 不受影响。
        fixture.write_u32(usize::from(first) * DESC_BYTES + 12, u32::MAX);
        fixture.used(0, u32::from(second), 19);
        fixture.used(1, u32::from(first), 31);
        fixture.publish_used(2);
        assert_eq!(
            fixture.queue.pop_used(),
            Ok(Some(UsedElement {
                head: second,
                len: 19
            }))
        );
        assert_eq!(fixture.queue.available_descriptors(), 5);
        assert_eq!(
            fixture.queue.pop_used(),
            Ok(Some(UsedElement {
                head: first,
                len: 31
            }))
        );
        assert_eq!(fixture.queue.available_descriptors(), 8);
        let reused = fixture.submit(4);
        fixture.used(2, u32::from(reused), 512);
        fixture.publish_used(3);
        assert_eq!(
            fixture.queue.pop_used(),
            Ok(Some(UsedElement {
                head: reused,
                len: 512
            }))
        );
        assert_eq!(fixture.queue.available_descriptors(), 8);
    }

    /// 完整 32 位 id、链内节点及重复 used 均不能回收仍在途资源。
    #[test]
    fn invalid_and_duplicate_used_preserve_inflight() {
        let mut fixture = Fixture::new(4, false);
        let first = fixture.submit(2);
        let second = fixture.submit(1);
        fixture.used(0, 0x1_0000 + u32::from(first), 0);
        fixture.used(1, u32::from(first), 0);
        fixture.publish_used(2);
        assert_eq!(fixture.queue.pop_used(), Err(Error::WrongToken));
        assert_eq!(fixture.queue.available_descriptors(), 1);
        fixture.used(0, u32::from(first) + 1, 0);
        assert_eq!(fixture.queue.pop_used(), Err(Error::WrongToken));
        assert_eq!(fixture.queue.available_descriptors(), 1);
        fixture.used(0, u32::from(first), 7);
        assert_eq!(
            fixture.queue.pop_used(),
            Ok(Some(UsedElement {
                head: first,
                len: 7
            }))
        );
        assert_eq!(fixture.queue.pop_used(), Err(Error::WrongToken));
        assert_eq!(fixture.queue.available_descriptors(), 3);
        fixture.used(1, u32::from(second), 9);
        assert_eq!(
            fixture.queue.pop_used(),
            Ok(Some(UsedElement {
                head: second,
                len: 9
            }))
        );
        assert_eq!(fixture.queue.available_descriptors(), 4);
    }

    /// 非法 used 索引跨度不能被当成可消费完成。
    #[test]
    fn invalid_used_span_preserves_resources() {
        let mut fixture = Fixture::new(4, false);
        let head = fixture.submit(1);
        fixture.used(0, u32::from(head), 5);
        for invalid in [2, 5, u16::MAX] {
            fixture.publish_used(invalid);
            assert_eq!(fixture.queue.pop_used(), Err(Error::DeviceNeedsReset));
            assert_eq!(fixture.queue.available_descriptors(), 3);
        }
        fixture.publish_used(1);
        assert_eq!(
            fixture.queue.pop_used(),
            Ok(Some(UsedElement { head, len: 5 }))
        );
    }

    /// 额度不足或非法末段不会写半链、消耗 head 或发表 avail。
    #[test]
    fn failed_submission_is_transactional() {
        let mut fixture = Fixture::new(4, false);
        let head = fixture.submit(2);
        let segments = [fixture.segment(); 3];
        assert_eq!(
            // SAFETY: 所有片段都来自仍保活的只读 payload，失败不得交权。
            unsafe { fixture.queue.add(&[&segments]) },
            Err(Error::QueueFull)
        );
        let invalid = MappedSegment {
            address: DmaAddr::new(u64::MAX),
            length: 2,
            ..fixture.segment()
        };
        assert_eq!(
            // SAFETY: 此末端溢出在任何发表前被拒绝，不发生设备访问；合法首段仍保活。
            unsafe { fixture.queue.add(&[&segments[..1], &[invalid]]) },
            Err(Error::InvalidParam)
        );
        assert_eq!(fixture.queue.available_descriptors(), 2);
        assert_eq!(
            fixture
                .queue
                .read_u16(fixture.queue.layout.avail_offset + 2),
            1
        );
        let other = fixture.submit(2);
        assert_ne!(head, other);
        assert_eq!(fixture.queue.available_descriptors(), 0);
        fixture.used(0, u32::from(other), 2);
        fixture.used(1, u32::from(head), 1);
        fixture.publish_used(2);
        assert_eq!(
            fixture.queue.pop_used(),
            Ok(Some(UsedElement {
                head: other,
                len: 2
            }))
        );
        assert_eq!(
            fixture.queue.pop_used(),
            Ok(Some(UsedElement { head, len: 1 }))
        );
        assert_eq!(fixture.queue.available_descriptors(), 4);
    }

    /// avail 与 used 的 16 位环绕保持槽位、真实 head 及额度一致。
    #[test]
    fn counters_wrap_at_u16_boundary() {
        let mut fixture = Fixture::new(4, true);
        fixture.queue.avail_idx = u16::MAX - 1;
        fixture.queue.last_used_idx = u16::MAX - 1;
        fixture
            .queue
            .write_u16(fixture.queue.layout.avail_offset + 2, u16::MAX - 1);
        fixture.publish_used(u16::MAX - 1);
        let first = fixture.submit(1);
        let second = fixture.submit(1);
        assert_eq!(
            fixture
                .queue
                .read_u16(fixture.queue.layout.avail_offset + 2),
            0
        );
        fixture.used(u16::MAX - 1, u32::from(second), 8);
        fixture.used(u16::MAX, u32::from(first), 4);
        fixture.publish_used(0);
        assert_eq!(
            fixture.queue.pop_used(),
            Ok(Some(UsedElement {
                head: second,
                len: 8
            }))
        );
        assert_eq!(
            fixture.queue.pop_used(),
            Ok(Some(UsedElement {
                head: first,
                len: 4
            }))
        );
        assert_eq!(fixture.queue.pop_used(), Ok(None));
        assert_eq!(fixture.queue.available_descriptors(), 4);
    }

    /// flags 通知抑制及重装窗口内的新完成均通过真实 ring 字节观察。
    #[test]
    fn notification_rearm_rechecks_completion() {
        let mut fixture = Fixture::new(4, false);
        fixture.queue.set_interrupts(false);
        assert_eq!(
            fixture.queue.read_u16(fixture.queue.layout.avail_offset),
            NO_INTERRUPT
        );
        assert!(!fixture.queue.enable_interrupts_and_recheck());
        let head = fixture.submit(1);
        fixture
            .queue
            .write_u16(fixture.queue.layout.used_offset, NO_NOTIFY);
        assert!(!fixture.queue.should_notify());
        fixture.queue.write_u16(fixture.queue.layout.used_offset, 0);
        assert!(fixture.queue.should_notify());
        fixture.queue.set_interrupts(false);
        fixture.used(0, u32::from(head), 23);
        // SAFETY: used.idx 位于 Fixture 的页对齐真实映射内，指针只保留到紧接的
        // 同步 dma_barrier 回调，Fixture 不移动分配或结束其生命周期。
        let used_idx = unsafe {
            fixture
                .queue
                .region
                .cpu
                .as_ptr()
                .add(fixture.queue.layout.used_offset + 2)
                .cast::<u16>()
        };
        COMPLETE_AT_BARRIER.with(|completion| completion.set(Some((used_idx, 1))));
        assert!(fixture.queue.enable_interrupts_and_recheck());
        assert_eq!(fixture.queue.read_u16(fixture.queue.layout.avail_offset), 0);
        assert_eq!(
            fixture.queue.pop_used(),
            Ok(Some(UsedElement { head, len: 23 }))
        );
        assert!(!fixture.queue.enable_interrupts_and_recheck());
    }
}
