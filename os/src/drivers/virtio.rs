//! 选定块设备的真实传输、ring/PDU 页和停止责任。

#[cfg(feature = "ktest")]
pub(crate) mod ktest_prepare;

use crate::{
    boot::Region,
    drivers::{
        mmio_region_address,
        pci::{PciDevice, PciInventory},
        MmioBlockCandidate, ProbeError,
    },
    irq::IrqNumber,
    mm::dma::{CoherentDma, DmaError, KernelHal},
    task,
};
use core::convert::TryFrom;
use core::{mem::ManuallyDrop, ptr::NonNull};
use virtio_drivers::{
    device::blk::{BlkInfo, VirtIOBlk, PDU_STRIDE},
    queue::{RingLayout, VirtQueue},
    transport::{
        mmio::{MmioError, MmioTransport, VirtIOHeader},
        pci::{
            bus::{DeviceFunction, PCI_CAP_ID_VNDR},
            PciRegion, PciTransport, SelectedConstruct, VirtioPciError, VIRTIO_VENDOR_ID,
        },
        DeviceStatus, DeviceType, InterruptStatus, SomeTransport, Transport,
    },
    DmaRegion, Error, Hal,
};

/// 保留真实资源、传输与 DMA 准备失败，不把资源不足当无设备。
#[derive(Debug)]
pub enum VirtioInitError {
    NoMemory {
        status: DeviceStatus,
    },
    Resource(ProbeError),
    Mmio(MmioError),
    Pci(VirtioPciError),
    Dma {
        error: DmaError,
        status: DeviceStatus,
    },
    Protocol(Error),
}

impl From<ProbeError> for VirtioInitError {
    /// 保存库存与映射边界的实际失败。
    fn from(error: ProbeError) -> Self {
        Self::Resource(error)
    }
}
impl From<MmioError> for VirtioInitError {
    /// 保留 MMIO 身份和版本错误。
    fn from(error: MmioError) -> Self {
        Self::Mmio(error)
    }
}
impl From<VirtioPciError> for VirtioInitError {
    /// 保留选定 PCI capability 的实际错误。
    fn from(error: VirtioPciError) -> Self {
        Self::Pci(error)
    }
}
impl VirtioInitError {
    /// 页分配失败在 transport 仍存活时保留真实状态，不以重新 reset 后的值代替。
    fn dma_prepare(error: DmaError, status: DeviceStatus) -> Self {
        if error == DmaError::NoMemory {
            Self::NoMemory { status }
        } else {
            Self::Dma { error, status }
        }
    }
    /// shadow 分配失败同样保留未发表时的设备状态。
    fn queue_prepare(error: Error, status: DeviceStatus) -> Self {
        if error == Error::NoMemory {
            Self::NoMemory { status }
        } else {
            Self::Protocol(error)
        }
    }
}
impl From<Error> for VirtioInitError {
    /// 无设备状态上下文的协议错误保持原值，不伪造状态快照。
    fn from(error: Error) -> Self {
        Self::Protocol(error)
    }
}

/// 完整硬件身份；MMIO 物理窗口与 PCI BDF 不混用地址域。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VirtioIdentity {
    Mmio {
        physical: usize,
        version: u32,
        vendor_id: u32,
    },
    Pci {
        bus: u8,
        device: u8,
        function: u8,
        vendor_id: u16,
        device_id: u16,
    },
}

/// 原页拥有者和库 driver 共存；未确认停止时禁止析构任何控制区。
pub struct VirtioDevice {
    driver: ManuallyDrop<VirtIOBlk<KernelHal, SomeTransport<'static>>>,
    ring: ManuallyDrop<CoherentDma>,
    pdu: ManuallyDrop<CoherentDma>,
    identity: VirtioIdentity,
    interrupt: IrqNumber,
    must_reset: bool,
}

/// 仅验收镜像读取仍由本实例拥有的 transport 与 ring，不更改设备状态。
#[cfg(any(feature = "storage-tests", feature = "ktest"))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct TestPrepareSnapshot {
    pub identity: VirtioIdentity,
    pub status: DeviceStatus,
    pub avail_index: u16,
    pub available_descriptors: usize,
    pub requires_reset: bool,
    pub irq_actions: usize,
}

impl VirtioDevice {
    /// 在普通 Task 中准备已发现 MMIO 块设备，不发表队列或 DRIVER_OK。
    ///
    /// # Safety
    /// 调用者须为该硬件的唯一 owner；candidate 的完整物理窗口必须已永久强序映射，
    /// 设备身份来自同一次真实发现，且不能与另一个 transport 同时使用该设备。
    pub unsafe fn prepare_mmio(candidate: &MmioBlockCandidate) -> Result<Self, VirtioInitError> {
        drop(task::current_task());
        let base = mmio_region_address(candidate.registers)?;
        let size = candidate
            .registers
            .end
            .checked_sub(candidate.registers.start)
            .ok_or(ProbeError::Resource("invalid MMIO extent"))?;
        let header = NonNull::new(base as *mut VirtIOHeader)
            .ok_or(ProbeError::Resource("null MMIO register address"))?;
        // SAFETY: 调用者保证实际唯一设备与完整永久映射；构造再检查长度、对齐和身份。
        let transport = unsafe { MmioTransport::new(header, size) }?;
        if u32::from(transport.version()) != candidate.version
            || transport.vendor_id() != candidate.vendor_id
        {
            return Err(ProbeError::Resource("MMIO inventory identity changed").into());
        }
        Self::prepare(
            transport.into(),
            VirtioIdentity::Mmio {
                physical: candidate.registers.start,
                version: candidate.version,
                vendor_id: candidate.vendor_id,
            },
            candidate.interrupt,
        )
    }

    /// 只消费库存已选 BDF、cap 元数据与 BAR，不重新扫描或创建配置 owner。
    ///
    /// # Safety
    /// 调用者须为此硬件的唯一 owner，库存 BAR 的完整 CPU 物理窗口必须已永久强序映射；
    /// 设备及映射须存活至所有通知/IRQ 尾部结束和确认 reset。
    pub unsafe fn prepare_pci(
        inventory: &PciInventory,
        device: &PciDevice,
    ) -> Result<Self, VirtioInitError> {
        drop(task::current_task());
        let selected = selected_construct(inventory, device)?;
        let interrupt = device.interrupt.ok_or(ProbeError::Resource(
            "VirtIO PCI device has no INTx resource",
        ))?;
        // SAFETY: 已选数据来自本库存唯一功能；调用者承担完整映射、设备寿命及独占义务。
        let transport = unsafe { PciTransport::new(selected) }?;
        Self::prepare(
            transport.into(),
            VirtioIdentity::Pci {
                bus: device.bus,
                device: device.device,
                function: device.function,
                vendor_id: device.vendor_id,
                device_id: device.device_id,
            },
            interrupt,
        )
    }

    /// reset 确认后锁外预备所有 fallible 资源，尚未发表时错误自动归还原页。
    fn prepare(
        mut transport: SomeTransport<'static>,
        identity: VirtioIdentity,
        interrupt: IrqNumber,
    ) -> Result<Self, VirtioInitError> {
        if transport.device_type() != DeviceType::Block {
            return Err(ProbeError::Resource("selected VirtIO device is not block").into());
        }
        transport.request_reset();
        if !transport.reset_complete() {
            return Err(Error::ResetPending.into());
        }
        KernelHal::dma_barrier();
        let info = VirtIOBlk::<KernelHal, SomeTransport<'static>>::negotiate(&mut transport)?;
        let layout = RingLayout::new(info.queue_depth, transport.requires_legacy_layout())?;
        let ring = CoherentDma::allocate(layout.byte_len())
            .map_err(|error| VirtioInitError::dma_prepare(error, transport.get_status()))?;
        let pdu_size = usize::from(info.tag_count)
            .checked_mul(PDU_STRIDE)
            .ok_or(Error::InvalidParam)?;
        let pdu = CoherentDma::allocate(pdu_size)
            .map_err(|error| VirtioInitError::dma_prepare(error, transport.get_status()))?;
        // SAFETY: 原 ring owner 存活，真实 coherent CPU/DMA 同一页，尚未交设备访问。
        let queue = unsafe { VirtQueue::<KernelHal>::new(layout, ring.region()) }
            .map_err(|error| VirtioInitError::queue_prepare(error, transport.get_status()))?;
        // SAFETY: ring/PDU 原页与 driver 同寿命，绑定不发表；拥有者在发表后强制真实 reset。
        let driver = unsafe { VirtIOBlk::new(transport, info, queue, pdu.region()) }?;
        Ok(Self {
            driver: ManuallyDrop::new(driver),
            ring: ManuallyDrop::new(ring),
            pdu: ManuallyDrop::new(pdu),
            identity,
            interrupt,
            must_reset: false,
        })
    }

    /// 只读访问实际 driver；不返回第二份拥有者。
    pub fn driver(&self) -> &VirtIOBlk<KernelHal, SomeTransport<'static>> {
        &self.driver
    }
    /// 在实际 owner 保活期间借用后端并保留停止责任，调用者负责队列和通知的串行化。
    ///
    /// # Safety
    /// 不得移出、替换或交换后端；它必须始终关联本 owner 的硬件及 ring/PDU。
    /// 借用期间须保证对应请求的全部 DMA 拥有者存活，且不与 reset 或另一个访问者并行。
    pub unsafe fn driver_mut(&mut self) -> &mut VirtIOBlk<KernelHal, SomeTransport<'static>> {
        self.must_reset = true;
        &mut self.driver
    }
    /// 返回真实协商所得块设备事实。
    pub fn info(&self) -> BlkInfo {
        self.driver.info()
    }
    /// 返回第一批 IRQ domain 的真实身份。
    pub fn interrupt(&self) -> IrqNumber {
        self.interrupt
    }
    /// 返回不可变完整硬件身份。
    pub fn identity(&self) -> VirtioIdentity {
        self.identity
    }
    /// 返回原 ring/PDU 分配的裸视图；不提供安全 CPU 解引用或另一份页拥有权。
    pub fn control_regions(&self) -> (DmaRegion, DmaRegion) {
        (self.ring.region(), self.pdu.region())
    }
    /// 返回真实硬件状态，供初始化诊断和停止流程使用。
    pub fn status(&self) -> DeviceStatus {
        self.driver.status()
    }

    /// 在真实准备失败点、实例销毁前读取硬件 status 与本实例原 ring 的 avail.idx。
    #[cfg(any(feature = "storage-tests", feature = "ktest"))]
    pub(crate) fn test_prepare_snapshot(&self) -> TestPrepareSnapshot {
        let depth = self.info().queue_depth;
        let ring = self.ring.region();
        // 两种 split 布局的 avail 都紧接 descriptor 表，legacy 仅改变 used 对齐。
        let avail_index_offset = usize::from(depth) * 16 + 2;
        assert!(avail_index_offset + core::mem::size_of::<u16>() <= ring.len);
        KernelHal::dma_read_barrier();
        // SAFETY: 本方法借用唯一实例，ring 原页仍 live；offset 是已验证 split
        // 布局内对齐的 u16，volatile 读取不创建 DMA 内存引用，不消费或修改队列。
        let avail_index = u16::from_le(unsafe {
            core::ptr::read_volatile(ring.cpu.as_ptr().add(avail_index_offset).cast::<u16>())
        });
        TestPrepareSnapshot {
            identity: self.identity,
            status: self.status(),
            avail_index,
            available_descriptors: self.driver.available_descriptors(),
            requires_reset: self.must_reset,
            irq_actions: crate::irq::test_action_count(self.interrupt),
        }
    }
    /// 确认本设备 ISR，不重新引入 DMA 停止责任；退休期仍须清本设备共享 IRQ 原因。
    pub fn ack_interrupt(&mut self) -> InterruptStatus {
        self.driver.ack_interrupt()
    }
    /// 在任何 queue_set 写入前标记停止责任；失败返回时仍保完整原页和 driver。
    pub fn publish(&mut self) -> Result<(), VirtioInitError> {
        self.must_reset = true;
        KernelHal::dma_barrier();
        self.driver.publish().map_err(Into::into)
    }
    /// 请求设备停止，写零不清除停止责任。
    pub fn request_reset(&mut self) {
        self.must_reset = true;
        KernelHal::dma_barrier();
        self.driver.request_reset();
    }
    /// 一次真实状态读回为零后执行完整屏障并清停止责任；未完成时保持全部 owner。
    pub fn reset_complete(&mut self) -> bool {
        if !self.driver.reset_complete() {
            return false;
        }
        KernelHal::dma_barrier();
        self.must_reset = false;
        true
    }
    /// 查询当前是否必须先确认设备停止；不代替真实状态读回。
    pub fn requires_reset(&self) -> bool {
        self.must_reset
    }
}

impl Drop for VirtioDevice {
    /// 仅确认停止或从未发表时销毁原 driver 和控制页；无 reset 写入或忙等。
    fn drop(&mut self) {
        assert!(
            !self.must_reset,
            "VirtIO control DMA dropped before confirmed reset"
        );
        // SAFETY: 从未发表或 reset 已真实确认并完整屏障；driver 先销毁，原页随后各归还一次。
        unsafe {
            ManuallyDrop::drop(&mut self.driver);
            ManuallyDrop::drop(&mut self.pdu);
            ManuallyDrop::drop(&mut self.ring);
        }
    }
}

/// 读取同一次库存的选定 capability 字段，checked 偏移不能回绕到身份配置字。
fn cap_word(
    inventory: &PciInventory,
    device: &PciDevice,
    cap: u8,
    field: u8,
) -> Result<u32, VirtioInitError> {
    let offset = cap
        .checked_add(field)
        .ok_or(ProbeError::Resource("PCI capability offset overflows"))?;
    Ok(inventory.read_config_word(device, offset)?)
}

/// 将 capability 的 BAR 内范围换算为已经映射的强序 CPU 窗口。
fn capability_region(
    device: &PciDevice,
    bar: u8,
    offset: u32,
    length: u32,
) -> Result<PciRegion, VirtioInitError> {
    let bar = device
        .bars
        .get(usize::from(bar))
        .and_then(Option::as_ref)
        .ok_or(ProbeError::Resource(
            "VirtIO capability references unassigned memory BAR",
        ))?;
    let offset = usize::try_from(offset)
        .map_err(|_| ProbeError::Resource("PCI capability offset is not addressable"))?;
    let len = usize::try_from(length)
        .map_err(|_| ProbeError::Resource("PCI capability length is not addressable"))?;
    let size = bar
        .physical
        .end
        .checked_sub(bar.physical.start)
        .ok_or(ProbeError::Resource("invalid PCI BAR physical extent"))?;
    let end = offset
        .checked_add(len)
        .ok_or(ProbeError::Resource("PCI capability extent overflows"))?;
    if len == 0 || size < end {
        return Err(ProbeError::Resource("PCI capability is outside assigned BAR").into());
    }
    let start = bar
        .physical
        .start
        .checked_add(offset)
        .ok_or(ProbeError::Resource("PCI capability address overflows"))?;
    let physical_end = start
        .checked_add(len)
        .ok_or(ProbeError::Resource("PCI capability end overflows"))?;
    let base = mmio_region_address(Region {
        start,
        end: physical_end,
    })?;
    Ok(PciRegion {
        base: NonNull::new(base as *mut u8)
            .ok_or(ProbeError::Resource("null PCI capability VA"))?,
        len,
    })
}

/// 使用已有 cap 数组选择四种协议窗口，不遍历硬件链或探测 BAR。
fn selected_construct(
    inventory: &PciInventory,
    device: &PciDevice,
) -> Result<SelectedConstruct, VirtioInitError> {
    let identity = inventory.read_config_word(device, 0)?;
    if identity as u16 != device.vendor_id
        || (identity >> 16) as u16 != device.device_id
        || device.vendor_id != VIRTIO_VENDOR_ID
    {
        return Err(ProbeError::Resource("PCI inventory identity changed").into());
    }
    let caps = device
        .capabilities
        .get(..device.capability_count)
        .ok_or(ProbeError::Resource(
            "PCI capability inventory count is invalid",
        ))?;
    let mut regions = [None; 4];
    let mut notify_off_multiplier = 0;
    for cap in caps.iter().flatten() {
        if cap.id != PCI_CAP_ID_VNDR {
            continue;
        }
        let length = cap.private_header as u8;
        let kind = (cap.private_header >> 8) as u8;
        if !(1..=4).contains(&kind) {
            continue;
        }
        if length < 16
            || (kind == 2 && length < 20)
            || 256 < usize::from(cap.offset) + usize::from(length)
        {
            return Err(ProbeError::Resource("short VirtIO PCI capability").into());
        }
        let index = usize::from(kind - 1);
        if regions[index].is_some() {
            continue;
        }
        let bar = cap_word(inventory, device, cap.offset, 4)? as u8;
        let offset = cap_word(inventory, device, cap.offset, 8)?;
        let region_length = cap_word(inventory, device, cap.offset, 12)?;
        regions[index] = Some(capability_region(device, bar, offset, region_length)?);
        if kind == 2 {
            notify_off_multiplier = cap_word(inventory, device, cap.offset, 16)?;
        }
    }
    Ok(SelectedConstruct {
        device_function: DeviceFunction {
            bus: device.bus,
            device: device.device,
            function: device.function,
        },
        vendor_id: device.vendor_id,
        device_id: device.device_id,
        common_cfg: regions[0].ok_or(VirtioPciError::MissingCommonConfig)?,
        notify_cfg: regions[1].ok_or(VirtioPciError::MissingNotifyConfig)?,
        notify_off_multiplier,
        isr_cfg: regions[2].ok_or(VirtioPciError::MissingIsrConfig)?,
        device_cfg: regions[3],
    })
}
