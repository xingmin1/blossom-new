//! 已映射 SG 上的 VirtIO 网络协议；原页与请求拥有关系由内核持有。

use crate::queue::{UsedElement, VirtQueue};
use crate::transport::{DeviceStatus, DeviceType, InterruptStatus, Notification, Transport};
use crate::{BufferDirection, Error, Hal, MappedSegment, Result};

const QUEUE_RECEIVE: u16 = 0;
const QUEUE_TRANSMIT: u16 = 1;
const MAC: u64 = 1 << 5;
const VERSION_1: u64 = 1 << 32;
const MIN_FRAME_LEN: usize = 14;
const MAX_FRAME_LEN: usize = 1514;
const LEGACY_HEADER_LEN: usize = 10;
const MODERN_HEADER_LEN: usize = 12;

/// 实际协商的 MAC、协议头长度与两个独立 split ring 的深度。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetInfo {
    /// 设备配置给出的单播以太网地址。
    pub mac: [u8; 6],
    /// legacy 为 10，现代 VERSION_1 为 12，与未选择的 merge 特性无关。
    pub header_len: usize,
    /// RX0 的 descriptor 深度；每帧两段，拥有者最多准备 min(32, depth / 2) 个槽。
    pub rx_depth: u16,
    /// TX1 的 descriptor 深度；head 命名空间与 RX0 独立。
    pub tx_depth: u16,
}

/// 一对 RX0/TX1 队列的非拥有控制视图，不分配 payload、不通知或等待完成。
/// 析构只结束 CPU 控制视图，设备停止和原页归还由外部拥有者负责。
pub struct VirtIONet<H: Hal, T: Transport> {
    transport: T,
    info: NetInfo,
    rx: VirtQueue<H>,
    tx: VirtQueue<H>,
    notifications: Option<(Notification, Notification)>,
}

impl<H: Hal, T: Transport> VirtIONet<H, T> {
    /// 在已确认 reset 的网络设备上协商 MAC 和现代必需的 VERSION_1。
    /// 不发表队列或 DRIVER_OK；配置和队列深度不可用时保留真实错误。
    pub fn negotiate(transport: &mut T) -> Result<NetInfo> {
        if transport.device_type() != DeviceType::Network {
            return Err(Error::Unsupported);
        }
        let features = transport.begin_init(MAC)?;
        if features & MAC == 0 {
            return Err(Error::Unsupported);
        }
        let header_len = if features & VERSION_1 == 0 {
            LEGACY_HEADER_LEN
        } else {
            MODERN_HEADER_LEN
        };
        let rx_depth = queue_depth(transport.max_queue_size(QUEUE_RECEIVE))?;
        let tx_depth = queue_depth(transport.max_queue_size(QUEUE_TRANSMIT))?;
        let mac = transport.read_consistent(|| {
            let mut mac = [0; 6];
            for (offset, byte) in mac.iter_mut().enumerate() {
                *byte = transport.read_config_space::<u8>(offset)?;
            }
            Ok(mac)
        })?;
        if !valid_mac(mac) {
            return Err(Error::InvalidParam);
        }
        Ok(NetInfo {
            mac,
            header_len,
            rx_depth,
            tx_depth,
        })
    }

    /// 绑定本次协商的两个未发表 ring，不向设备写入地址。
    ///
    /// # Safety
    /// info 必须来自同一 transport 的本次 negotiate，rx/tx 必须采用该 transport
    /// 的真实布局且互不重叠，当前没有设备或其它 CPU 使用队列。
    /// 两个 ring 的原页与 CPU 映射必须覆盖本对象及其后续持有者的全部 CPU 访问，
    /// 通常保活至本对象销毁；归还前还须另外确认设备已经停止 DMA。
    /// 调用者必须独占设备及 driver-owned 字段，不得建立覆盖设备可写 ring 的引用。
    /// 析构不会请求 reset、等待完成或归还原页。
    pub unsafe fn new(
        mut transport: T,
        info: NetInfo,
        rx: VirtQueue<H>,
        tx: VirtQueue<H>,
    ) -> Result<Self> {
        let header_len = if transport.requires_legacy_layout() {
            LEGACY_HEADER_LEN
        } else {
            MODERN_HEADER_LEN
        };
        if transport.device_type() != DeviceType::Network
            || !valid_mac(info.mac)
            || info.header_len != header_len
            || info.rx_depth != queue_depth(transport.max_queue_size(QUEUE_RECEIVE))?
            || info.tx_depth != queue_depth(transport.max_queue_size(QUEUE_TRANSMIT))?
            || info.rx_depth != rx.depth()
            || info.tx_depth != tx.depth()
            || rx.available_descriptors() != usize::from(info.rx_depth)
            || tx.available_descriptors() != usize::from(info.tx_depth)
        {
            return Err(Error::InvalidParam);
        }
        Ok(Self {
            transport,
            info,
            rx,
            tx,
            notifications: None,
        })
    }

    /// 返回本设备实际协商事实，不推导上层当前可用的 payload 槽数。
    pub fn info(&self) -> NetInfo {
        self.info
    }

    /// 先准备两个 doorbell，再依次发表 RX0/TX1，最后确认 DRIVER_OK。
    /// 调用者须在第一次发表尝试前承担 reset 责任，错误可能已经留下活 DMA 地址。
    pub fn publish(&mut self) -> Result<()> {
        if self.notifications.is_some() {
            return Err(Error::AlreadyUsed);
        }
        let rx_notification = self.transport.notification(QUEUE_RECEIVE)?;
        let tx_notification = self.transport.notification(QUEUE_TRANSMIT)?;
        let (rx_descriptors, rx_driver, rx_device) = self.rx.addresses();
        let (tx_descriptors, tx_driver, tx_device) = self.tx.addresses();
        H::dma_barrier();
        self.transport.queue_set(
            QUEUE_RECEIVE,
            u32::from(self.rx.depth()),
            rx_descriptors,
            rx_driver,
            rx_device,
        )?;
        self.transport.queue_set(
            QUEUE_TRANSMIT,
            u32::from(self.tx.depth()),
            tx_descriptors,
            tx_driver,
            tx_device,
        )?;
        self.transport.finish_init()?;
        self.notifications = Some((rx_notification, tx_notification));
        Ok(())
    }

    /// 将完整 RX 映射拆成独立协议头和接收帧容量，返回 RX0 的真实 head。
    /// 不分配、不访问 payload 字节、不通知或等待；额度不足不发表新链。
    ///
    /// # Safety
    /// payload 必须已按 DeviceToDriver 交权且完整映射至少 header_len + 1514 字节，
    /// 不得与 ring、其它活动 payload 或 CPU 别名重叠。原页及映射须保活到该 RX
    /// head 的真实 used 被取出或确认 reset；此期间 CPU 不得访问 payload。
    /// 调用者须串行化队列操作，并在释放队列保护前安装 RX head 到唯一槽的拥有记录。
    /// 失败未发表新链，调用者仍须处理此前已经交权的映射。
    pub unsafe fn submit_rx(&mut self, payload: MappedSegment) -> Result<u16> {
        if self.notifications.is_none() {
            return Err(Error::NotReady);
        }
        if payload.direction != BufferDirection::DeviceToDriver
            || (payload.length as usize) < self.info.header_len + MAX_FRAME_LEN
        {
            return Err(Error::InvalidParam);
        }
        let segments = packet_segments(self.info.header_len, payload, payload.length as usize)?;
        // SAFETY: 调用者承担 RX 完整映射、交权、保活和 head 拥有记录义务；
        // 两段均来自同一已检查范围，仅复制设备地址元数据，不建立 payload 引用。
        unsafe { self.rx.add(&[&segments]) }
    }

    /// 仅提交本次真实 Ethernet 帧长度，返回 TX1 的真实 head，不读取 TX used.len。
    /// 协议头和帧始终是两段，legacy 未协商 ANY_LAYOUT 也保持此布局。
    ///
    /// # Safety
    /// payload 必须已按 DriverToDevice 交权，完整覆盖 header_len + frame_len，整个
    /// ToDevice 范围均已初始化。协议头须始终全零，CPU 独占阶段只编码本次帧，
    /// 交权后不得修改内容。映射不能与 ring 或其它活动 payload 非法别名，原页及
    /// 映射须保活至该 TX head 的 used 或确认 reset。所有队列操作须串行化，
    /// 成功后在释放队列保护前安装独立 TX head 到唯一槽和本次 frame_len 的记录。
    /// 失败未发表新链，调用者仍须处理此前已经交权的映射。
    pub unsafe fn submit_tx(&mut self, payload: MappedSegment, frame_len: usize) -> Result<u16> {
        if self.notifications.is_none() {
            return Err(Error::NotReady);
        }
        if payload.direction != BufferDirection::DriverToDevice
            || !(MIN_FRAME_LEN..=MAX_FRAME_LEN).contains(&frame_len)
        {
            return Err(Error::InvalidParam);
        }
        let segments = packet_segments(
            self.info.header_len,
            payload,
            self.info.header_len + frame_len,
        )?;
        // SAFETY: 调用者保证本次全零头、已初始化帧和 ToDevice 交权保活；
        // 范围已核，两段元数据仅由 queue.add 复制到 descriptor，不借用 payload。
        unsafe { self.tx.add(&[&segments]) }
    }

    /// 从 RX0 可信 shadow 消费真实 head，used.len 的前缀与协议头由收权后的拥有者验证。
    pub fn pop_rx_used(&mut self) -> Result<Option<UsedElement>> {
        self.rx.pop_used()
    }

    /// 从 TX1 可信 shadow 消费真实 head；used.len 可以为零，不表示发送字节数。
    pub fn pop_tx_used(&mut self) -> Result<Option<UsedElement>> {
        self.tx.pop_used()
    }

    /// 只读 RX0 已发表但尚未消费的真实 used 数量。
    pub fn pending_rx_used(&self) -> Result<usize> {
        self.rx.pending_used()
    }

    /// 只读 TX1 已发表但尚未消费的真实 used 数量。
    pub fn pending_tx_used(&self) -> Result<usize> {
        self.tx.pending_used()
    }

    /// 返回 RX0 的实际 descriptor 额度，两个 descriptor 才能容纳一帧。
    pub fn available_rx_descriptors(&self) -> usize {
        self.rx.available_descriptors()
    }

    /// 返回 TX1 的实际 descriptor 额度，不混入上层 Preparing 槽数。
    pub fn available_tx_descriptors(&self) -> usize {
        self.tx.available_descriptors()
    }

    /// 同时更新两个真实 avail ring 的中断抑制标志。
    pub fn set_interrupts(&mut self, enabled: bool) {
        self.rx.set_interrupts(enabled);
        self.tx.set_interrupts(enabled);
    }

    /// 各自完成 RX0/TX1 重装和 used 复查后合并结果，不短路任何队列的重装。
    pub fn enable_interrupts_and_recheck(&mut self) -> bool {
        let rx_pending = self.rx.enable_interrupts_and_recheck();
        let tx_pending = self.tx.enable_interrupts_and_recheck();
        rx_pending || tx_pending
    }

    /// 返回成功 publish 的 RX0 doorbell；未发表或请求 reset 后调用会 panic。
    /// 锁外 unsafe notify 的设备、映射保活和停止串行化仍由内核承担。
    pub fn rx_notification(&self) -> Notification {
        self.notifications
            .expect("virtio net queues are not published")
            .0
    }

    /// 返回成功 publish 的 TX1 doorbell，不取得 RX0 的通知身份。
    pub fn tx_notification(&self) -> Notification {
        self.notifications
            .expect("virtio net queues are not published")
            .1
    }

    /// 完整排序 avail 发表后按 RX0 的设备 flags 判定是否需要通知。
    pub fn rx_should_notify(&self) -> bool {
        self.rx.should_notify()
    }

    /// 完整排序 avail 发表后按 TX1 的设备 flags 判定是否需要通知。
    pub fn tx_should_notify(&self) -> bool {
        self.tx.should_notify()
    }

    /// 读取并确认本设备 ISR，保留真实队列和配置原因供共享 IRQ 消费。
    pub fn ack_interrupt(&mut self) -> InterruptStatus {
        self.transport.ack_interrupt()
    }

    /// 撤销两队列提交及新通知视图的准入，再请求真实 reset，不等待或假定停止。
    /// 此前复制的 doorbell 仍须由拥有者同步结束所有通知尾部。
    pub fn request_reset(&mut self) {
        self.notifications = None;
        self.transport.request_reset();
    }

    /// 读取设备真实 reset 完成状态，不以请求写零代替确认。
    pub fn reset_complete(&self) -> bool {
        self.transport.reset_complete()
    }

    /// 返回实际 transport 状态，供拥有者执行真实停止和错误协议。
    pub fn status(&self) -> DeviceStatus {
        self.transport.get_status()
    }
}

/// 从已收权并初始化的本次真实 used 前缀中取得 Ethernet 帧，不解释映射尾部。
/// 调用者必须只传实际 used.len 覆盖的 CPU-owned 字节，所有借用须在重投 RX 前结束。
/// DATA_VALID 仅作可忽略提示，上层仍做软件校验；未选择 merge，不读取 num_buffers。
pub fn rx_frame(payload: &[u8], header_len: usize) -> Result<&[u8]> {
    if ![LEGACY_HEADER_LEN, MODERN_HEADER_LEN].contains(&header_len) {
        return Err(Error::InvalidParam);
    }
    if payload.len() < header_len + MIN_FRAME_LEN || header_len + MAX_FRAME_LEN < payload.len() {
        return Err(Error::IoError);
    }
    if payload[0] & 1 != 0 || payload[1] != 0 {
        return Err(Error::Unsupported);
    }
    Ok(&payload[header_len..])
}

/// 将设备 queue max 限于 64 并向下取合法二次幂，至少容纳两个两段帧。
fn queue_depth(maximum: u32) -> Result<u16> {
    let maximum = maximum.min(64);
    if maximum < 4 {
        return Err(Error::InvalidParam);
    }
    Ok((1_u32 << (31 - maximum.leading_zeros())) as u16)
}

/// 拒绝零地址和组播地址，不为不可用的设备身份生成替代 MAC。
fn valid_mac(mac: [u8; 6]) -> bool {
    mac != [0; 6] && mac[0] & 1 == 0
}

/// 在发表前核对真实范围，构造首段恰为协议头的两段映射元数据。
fn packet_segments(
    header_len: usize,
    payload: MappedSegment,
    total_len: usize,
) -> Result<[MappedSegment; 2]> {
    if total_len <= header_len
        || (payload.length as usize) < total_len
        || payload.address.checked_add(total_len - 1).is_none()
    {
        return Err(Error::InvalidParam);
    }
    let body_address = payload
        .address
        .checked_add(header_len)
        .ok_or(Error::InvalidParam)?;
    Ok([
        MappedSegment {
            address: payload.address,
            length: header_len as u32,
            direction: payload.direction,
        },
        MappedSegment {
            address: body_address,
            length: (total_len - header_len) as u32,
            direction: payload.direction,
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 检查真实 used 前缀、两种头长及未选择 offload 的边界，不用映射尾部充当帧。
    #[test]
    fn receive_prefix_boundaries() {
        for header_len in [LEGACY_HEADER_LEN, MODERN_HEADER_LEN] {
            let mut payload = [0; MODERN_HEADER_LEN + MAX_FRAME_LEN + 1];
            payload[header_len..header_len + MAX_FRAME_LEN].fill(0xa5);
            let prefix_len = header_len + MIN_FRAME_LEN;
            let frame = rx_frame(&payload[..prefix_len], header_len).unwrap();
            assert_eq!(frame.len(), MIN_FRAME_LEN);
            assert_eq!(frame[MIN_FRAME_LEN - 1], 0xa5);
            assert_eq!(
                rx_frame(&payload[..header_len + MAX_FRAME_LEN], header_len)
                    .unwrap()
                    .len(),
                MAX_FRAME_LEN
            );
            assert_eq!(
                rx_frame(&payload[..prefix_len - 1], header_len),
                Err(Error::IoError)
            );
            assert_eq!(
                rx_frame(&payload[..header_len + MAX_FRAME_LEN + 1], header_len),
                Err(Error::IoError)
            );
            payload[0] = 2;
            if header_len == MODERN_HEADER_LEN {
                payload[10] = 9;
                payload[11] = 0;
            }
            assert_eq!(
                rx_frame(&payload[..prefix_len], header_len).unwrap().len(),
                MIN_FRAME_LEN
            );
            payload[0] = 1;
            assert_eq!(
                rx_frame(&payload[..prefix_len], header_len),
                Err(Error::Unsupported)
            );
            payload[0] = 0;
            payload[1] = 1;
            assert_eq!(
                rx_frame(&payload[..prefix_len], header_len),
                Err(Error::Unsupported)
            );
        }
        assert_eq!(rx_frame(&[], 11), Err(Error::InvalidParam));
    }
}
