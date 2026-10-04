//! VirtIO 块设备协议后端；控制区与请求拥有关系由内核持有。

use crate::hal::{BufferDirection, DmaRegion, Hal, MappedSegment};
use crate::queue::{UsedElement, VirtQueue};
use crate::transport::{DeviceStatus, InterruptStatus, Notification, Transport};
use crate::{Error, Result};
use core::ptr::write_volatile;

const QUEUE: u16 = 0;
const SIZE_MAX: u64 = 1 << 1;
const SEG_MAX: u64 = 1 << 2;
const RO: u64 = 1 << 5;
const BLK_SIZE: u64 = 1 << 6;
const FLUSH: u64 = 1 << 9;
const SUPPORTED_FEATURES: u64 = SIZE_MAX | SEG_MAX | RO | BLK_SIZE | FLUSH;

/// 设备协议中的 sector 固定为 512 字节，独立于逻辑块大小。
pub const SECTOR_SIZE: usize = 512;
/// 每个 tag 独占的稳定控制区跨度。
pub const PDU_STRIDE: usize = 32;
/// 请求头包含 type、保留字段与 sector，共 16 字节。
pub const HEADER_SIZE: usize = 16;
/// 设备写入的原始状态字节位于请求头之后。
pub const STATUS_OFFSET: usize = 16;

/// 当前支持的三个块协议操作。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlkOperation {
    /// 设备写入请求的 payload 范围。
    Read,
    /// 设备读取请求的 payload 并写入介质。
    Write,
    /// 请求设备将写缓存提交到稳定存储。
    Flush,
}

/// 实际协商出的容量、协议限制与固定请求槽数量。
#[derive(Clone, Copy, Debug)]
pub struct BlkInfo {
    /// 设备容量，单位固定为 512 字节扇区。
    pub capacity_sectors: u64,
    /// 实际选择的 split-ring descriptor 深度。
    pub queue_depth: u16,
    /// 预备稳定 PDU 的请求槽数，与 descriptor 额度分别计量。
    pub tag_count: u16,
    /// 单个 payload SG 段的字节上限；协议无上限时为 u32::MAX。
    pub max_segment_bytes: u32,
    /// 一条命令的 payload SG 上限，不含 header/status。
    pub max_segments: u16,
    /// 普通数据请求必须遵守的设备逻辑块对齐字节数。
    pub logical_block_size: u32,
    /// 设备是否通过 RO feature 声明只读。
    pub readonly: bool,
    /// 是否协商了实际可提交的 FLUSH 命令。
    pub flush: bool,
}

/// 一个设备的一条 split queue；不拥有 DMA 页，也不保存核心请求表。
pub struct VirtIOBlk<H: Hal, T: Transport> {
    transport: T,
    info: BlkInfo,
    queue: VirtQueue<H>,
    pdu: DmaRegion,
    notification: Option<Notification>,
}

impl<H: Hal, T: Transport> VirtIOBlk<H, T> {
    /// 在已经确认 reset 的设备上协商实际使用的特性，并一致读取配置。
    /// 不发表队列，不置 DRIVER_OK；配置持续变化由 transport 有界返回错误。
    pub fn negotiate(transport: &mut T) -> Result<BlkInfo> {
        let features = transport.begin_init(SUPPORTED_FEATURES)?;
        let queue_max = transport.max_queue_size(QUEUE).min(128);
        if queue_max < 4 {
            return Err(Error::InvalidParam);
        }
        let queue_depth = (1_u32 << (31 - queue_max.leading_zeros())) as u16;
        // 配置偏移来自块协议 ABI；仅读取已经协商的可选字段。
        let (capacity, size_max, seg_max, block_size) = transport.read_consistent(|| {
            let low = u32::from_le(transport.read_config_space::<u32>(0)?);
            let high = u32::from_le(transport.read_config_space::<u32>(4)?);
            let capacity = u64::from(low) | (u64::from(high) << 32);
            let size_max = if features & SIZE_MAX != 0 {
                u32::from_le(transport.read_config_space::<u32>(8)?)
            } else {
                0
            };
            let seg_max = if features & SEG_MAX != 0 {
                u32::from_le(transport.read_config_space::<u32>(12)?)
            } else {
                1
            };
            let block_size = if features & BLK_SIZE != 0 {
                u32::from_le(transport.read_config_space::<u32>(20)?)
            } else {
                SECTOR_SIZE as u32
            };
            Ok((capacity, size_max, seg_max, block_size))
        })?;
        block_info(
            capacity,
            queue_depth,
            features,
            size_max,
            seg_max,
            block_size,
        )
    }

    /// 绑定已预备的真实 ring 与稳定 PDU，设备此时仍不得访问这些资源。
    ///
    /// # Safety
    /// 调用者必须保证 info 来自同一 transport 的本次协商，ring 与 PDU 不重叠，
    /// PDU 的 CPU/device 地址指向同一块 coherent 内存，整个范围可读写。
    /// ring/PDU 原页及 CPU 映射须覆盖本 driver 和其持有者的全部 CPU 访问，通常保活至 driver 销毁。
    /// 归还前还须确认设备已停止 DMA；仅确认 reset 不能提前释放仍被本对象访问的控制区。
    /// 不得并发访问 driver-owned 字段或创建与设备可写状态字节重叠的 Rust 引用。
    /// 库的析构不会停止设备。
    pub unsafe fn new(
        transport: T,
        info: BlkInfo,
        queue: VirtQueue<H>,
        pdu: DmaRegion,
    ) -> Result<Self> {
        let required = usize::from(info.tag_count)
            .checked_mul(PDU_STRIDE)
            .ok_or(Error::InvalidParam)?;
        if info.queue_depth != queue.depth()
            || info.tag_count != info.queue_depth.min(32)
            || required == 0
            || pdu.len < required
            || (isize::MAX as usize) < pdu.len
            || pdu.cpu.as_ptr() as usize % 16 != 0
            || pdu.device.value() % 16 != 0
            || (pdu.cpu.as_ptr() as usize)
                .checked_add(pdu.len - 1)
                .is_none()
            || pdu.device.checked_add(pdu.len - 1).is_none()
        {
            return Err(Error::InvalidParam);
        }
        Ok(Self {
            transport,
            info,
            queue,
            pdu,
            notification: None,
        })
    }

    /// 返回实际协商事实，供负责层构造队列限制。
    pub fn info(&self) -> BlkInfo {
        self.info
    }

    /// 全部资源绑定后，先验证 doorbell，再发表队列并置 DRIVER_OK。
    /// 设备拥有者必须在调用前承担 reset 责任；任何错误都可能已发表 DMA 地址。
    pub fn publish(&mut self) -> Result<()> {
        if self.notification.is_some() {
            return Err(Error::AlreadyUsed);
        }
        let notification = self.transport.notification(QUEUE)?;
        let (descriptors, driver_area, device_area) = self.queue.addresses();
        // 控制区初始化必须先于向设备发表其地址；该顺序由库边界保证。
        H::dma_barrier();
        self.transport.queue_set(
            QUEUE,
            u32::from(self.queue.depth()),
            descriptors,
            driver_area,
            device_area,
        )?;
        self.transport.finish_init()?;
        self.notification = Some(notification);
        Ok(())
    }

    /// 将锁外预备的稳定 tag PDU 与已映射 payload 写入 ring，返回真实 descriptor head。
    /// 此调用不分配、不改写 PDU、不通知、不等待；协议拒绝与额度不足原样返回。
    ///
    /// # Safety
    /// 调用者必须独占该 tag，保证它已无设备访问，并已用 prepare_pdu 写入本轮
    /// operation 与合法 sector。所有 payload owner 必须先交权并保活至 used 或
    /// 确认 reset；完整范围、对齐与能力由核心验证。成功后须在释放队列锁前
    /// 安装唯一 head→tag 记录；失败没有新链，调用者仍负责未发表范围的交权。
    pub unsafe fn submit(
        &mut self,
        tag: u16,
        operation: BlkOperation,
        payload: &[MappedSegment],
    ) -> Result<u16> {
        if self.notification.is_none() {
            return Err(Error::NotReady);
        }
        let offset = self.tag_offset(tag)?;
        let direction = match operation {
            BlkOperation::Read => BufferDirection::DeviceToDriver,
            BlkOperation::Write => {
                if self.info.readonly {
                    return Err(Error::Unsupported);
                }
                BufferDirection::DriverToDevice
            }
            BlkOperation::Flush => {
                if !self.info.flush {
                    return Err(Error::Unsupported);
                }
                if !payload.is_empty() {
                    return Err(Error::InvalidParam);
                }
                BufferDirection::DriverToDevice
            }
        };
        if (operation != BlkOperation::Flush && payload.is_empty())
            || usize::from(self.info.max_segments) < payload.len()
            || payload.iter().any(|segment| {
                segment.direction != direction || self.info.max_segment_bytes < segment.length
            })
        {
            return Err(Error::InvalidParam);
        }
        let device = self
            .pdu
            .device
            .checked_add(offset)
            .ok_or(Error::InvalidParam)?;
        let header = [MappedSegment {
            address: device,
            length: HEADER_SIZE as u32,
            direction: BufferDirection::DriverToDevice,
        }];
        let status = [MappedSegment {
            address: device
                .checked_add(STATUS_OFFSET)
                .ok_or(Error::InvalidParam)?,
            length: 1,
            direction: BufferDirection::DeviceToDriver,
        }];
        // SAFETY: 控制区满足 new 的保活约定；payload 的交权、方向与保活由调用者保证。
        // 临时数组仅保存地址元数据，queue.add 将真实设备地址复制进 descriptor。
        unsafe { self.queue.add(&[&header, payload, &status]) }
    }

    /// 消费并验证真实 used head，descriptor 仅按队列可信 shadow 归还。
    pub fn pop_used(&mut self) -> Result<Option<UsedElement>> {
        self.queue.pop_used()
    }

    /// 返回实际 ring 深度。
    pub fn queue_depth(&self) -> u16 {
        self.queue.depth()
    }

    /// 返回当前可提交的 descriptor 数量，不推导核心 tag 额度。
    pub fn available_descriptors(&self) -> usize {
        self.queue.available_descriptors()
    }

    /// 更新真实 avail 中断抑制标志。
    pub fn set_interrupts(&mut self, enabled: bool) {
        self.queue.set_interrupts(enabled);
    }

    /// 重装完成通知并复查 used，返回是否已有未处理完成。
    pub fn enable_interrupts_and_recheck(&mut self) -> bool {
        self.queue.enable_interrupts_and_recheck()
    }

    /// 在 ring 已发表后按设备标志判定是否需要通知。
    pub fn should_notify(&self) -> bool {
        self.queue.should_notify()
    }

    /// 返回成功 publish 时缓存的唯一 doorbell；未发表或 reset 后调用会 panic。
    /// 实际 notify 的 unsafe 设备/MMIO 保活义务仍由通知调用者承担。
    pub fn notification(&self) -> Notification {
        self.notification
            .expect("virtio block queue is not published")
    }

    /// 读取并确认本设备 ISR，使共享 IRQ 能区分队列与配置事件。
    pub fn ack_interrupt(&mut self) -> InterruptStatus {
        self.transport.ack_interrupt()
    }

    /// 撤销本对象的提交准入并请求真实 reset，不把寄存器写入视为停止完成。
    pub fn request_reset(&mut self) {
        self.notification = None;
        self.transport.request_reset();
    }

    /// 查询设备是否已经真实报告 reset 完成，不等待。
    pub fn reset_complete(&self) -> bool {
        self.transport.reset_complete()
    }

    /// 返回实际 transport 状态，供设备拥有者执行停止与故障协议。
    pub fn status(&self) -> DeviceStatus {
        self.transport.get_status()
    }

    /// 在原始指针运算前验证 tag，避免将核心输入当作控制区下标。
    fn tag_offset(&self, tag: u16) -> Result<usize> {
        if self.info.tag_count <= tag {
            return Err(Error::InvalidParam);
        }
        Ok(usize::from(tag) * PDU_STRIDE)
    }
}

/// 在队列锁外写入本轮独占 tag 的稳定请求头和原始未完成状态。
///
/// # Safety
/// region 必须是已验证的真实 coherent PDU，原页及 CPU 映射须覆盖所有访问；
/// 调用者独占此 tag，设备及其他 CPU 均不能访问本槽。operation/sector 必须
/// 已通过核心准入，后续 submit 使用同一 tag、operation 与同一个 PDU 区。
pub unsafe fn prepare_pdu(
    region: DmaRegion,
    tag: u16,
    operation: BlkOperation,
    sector: u64,
) -> Result<()> {
    let offset = usize::from(tag)
        .checked_mul(PDU_STRIDE)
        .ok_or(Error::InvalidParam)?;
    let end = offset
        .checked_add(STATUS_OFFSET + 1)
        .ok_or(Error::InvalidParam)?;
    if end > region.len
        || region.cpu.as_ptr() as usize % 16 != 0
        || (region.cpu.as_ptr() as usize)
            .checked_add(end - 1)
            .is_none()
        || region.device.value() % 16 != 0
        || region.device.checked_add(end - 1).is_none()
    {
        return Err(Error::InvalidParam);
    }
    let request_type = match operation {
        BlkOperation::Read => 0_u32,
        BlkOperation::Write => 1,
        BlkOperation::Flush => {
            if sector != 0 {
                return Err(Error::InvalidParam);
            }
            4
        }
    };
    // SAFETY: 完整 CPU 范围及 tag 对齐已核对；调用者保证独占原始槽和设备未访问。
    // 原始小端整数及 u8 状态不把任意设备写入解释为 Rust enum。
    unsafe {
        let cpu = region.cpu.as_ptr().add(offset);
        write_volatile(cpu.cast::<u32>(), request_type.to_le());
        write_volatile(cpu.add(4).cast::<u32>(), 0);
        write_volatile(cpu.add(8).cast::<u64>(), sector.to_le());
        write_volatile(cpu.add(STATUS_OFFSET), 0xff);
    }
    Ok(())
}

/// 将协议配置规范成有界事实；无容量或不可表示的字节几何直接拒绝。
fn block_info(
    capacity_sectors: u64,
    queue_depth: u16,
    features: u64,
    size_max: u32,
    seg_max: u32,
    logical_block_size: u32,
) -> Result<BlkInfo> {
    if capacity_sectors == 0
        || capacity_sectors.checked_mul(SECTOR_SIZE as u64).is_none()
        || seg_max == 0
        || logical_block_size < SECTOR_SIZE as u32
        || !logical_block_size.is_power_of_two()
    {
        return Err(Error::InvalidParam);
    }
    Ok(BlkInfo {
        capacity_sectors,
        queue_depth,
        tag_count: queue_depth.min(32),
        max_segment_bytes: if size_max == 0 { u32::MAX } else { size_max },
        max_segments: u32::from(queue_depth - 2).min(seg_max) as u16,
        logical_block_size,
        readonly: features & RO != 0,
        flush: features & FLUSH != 0,
    })
}

/// 状态保持 raw u8，任何未定义或未完成值都不能被当作成功。
pub fn decode_status(status: u8) -> Result<()> {
    match status {
        0 => Ok(()),
        2 => Err(Error::Unsupported),
        _ => Err(Error::IoError),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 设备可写任意状态字节，只有协议成功值可以完成请求为成功。
    #[test]
    fn raw_status_errors() {
        assert_eq!(decode_status(0), Ok(()));
        assert_eq!(decode_status(2), Err(Error::Unsupported));
        for status in [1, 3, 0x7f, 0xff] {
            assert_eq!(decode_status(status), Err(Error::IoError));
        }
    }

    /// 协议限制不接受零 SG、非法逻辑块或无法表示为字节范围的容量。
    #[test]
    fn limits_boundaries() {
        let info = block_info(8192, 128, RO | FLUSH, 0, u32::MAX, 4096).unwrap();
        assert_eq!(info.max_segments, 126);
        assert_eq!(info.max_segment_bytes, u32::MAX);
        assert_eq!(info.tag_count, 32);
        assert!(info.readonly && info.flush);
        for (capacity, segments, block_size) in [
            (0, 1, 512),
            (u64::MAX / 512 + 1, 1, 512),
            (8192, 0, 512),
            (8192, 1, 256),
            (8192, 1, 768),
        ] {
            assert!(matches!(
                block_info(capacity, 4, 0, 4096, segments, block_size),
                Err(Error::InvalidParam)
            ));
        }
    }
}
