#![allow(unused)]

use core::fmt;

use managed::{ManagedMap, ManagedSlice};

use crate::config::{FRAGMENTATION_BUFFER_SIZE, REASSEMBLY_BUFFER_COUNT, REASSEMBLY_BUFFER_SIZE};
use crate::storage::Assembler;
use crate::time::{Duration, Instant};
use crate::wire::*;

use core::result::Result;

#[cfg(feature = "alloc")]
type Buffer = alloc::vec::Vec<u8>;
#[cfg(not(feature = "alloc"))]
type Buffer = [u8; REASSEMBLY_BUFFER_SIZE];

/// 接口工作区准备失败；处理报文时不再扩展这些后备。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum BufferError {
    NoMemory,
    InvalidCapacity,
}

/// 在普通任务中一次准备发送分片和接收重组的实际后备。
pub struct InterfaceBuffers {
    pub(crate) fragments: FragmentsBuffer,
    pub(crate) fragmenter: Fragmenter,
}

impl InterfaceBuffers {
    /// 预备有限工作区；alloc 模式仅将 Vec 元数据留在调用栈上。
    pub fn try_new(
        fragmentation_capacity: usize,
        reassembly_capacity: usize,
    ) -> Result<Self, BufferError> {
        if fragmentation_capacity == 0 || reassembly_capacity == 0 {
            return Err(BufferError::InvalidCapacity);
        }
        #[cfg(not(feature = "alloc"))]
        if fragmentation_capacity != FRAGMENTATION_BUFFER_SIZE
            || reassembly_capacity != REASSEMBLY_BUFFER_SIZE
        {
            return Err(BufferError::InvalidCapacity);
        }
        Ok(Self {
            fragments: FragmentsBuffer {
                #[cfg(feature = "proto-sixlowpan")]
                decompress_buf: [0; MAX_DECOMPRESSED_LEN],
                #[cfg(feature = "_proto-fragmentation")]
                assembler: PacketAssemblerSet::try_new(reassembly_capacity)?,
                #[cfg(feature = "_proto-fragmentation")]
                reassembly_timeout: Duration::from_secs(60),
            },
            fragmenter: Fragmenter::try_new(fragmentation_capacity)?,
        })
    }
}

#[cfg(feature = "alloc")]
/// 只在构造期申请并初始化后备，容量错误与真实分配失败分开返回。
fn prepare_buffer(capacity: usize) -> Result<alloc::vec::Vec<u8>, BufferError> {
    if capacity == 0 {
        return Err(BufferError::InvalidCapacity);
    }
    let mut buffer = alloc::vec::Vec::new();
    buffer
        .try_reserve_exact(capacity)
        .map_err(|_| BufferError::NoMemory)?;
    buffer.resize(capacity, 0);
    Ok(buffer)
}

/// Problem when assembling: something was out of bounds.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct AssemblerError;

impl fmt::Display for AssemblerError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "AssemblerError")
    }
}

#[cfg(feature = "std")]
impl std::error::Error for AssemblerError {}

/// Packet assembler is full
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct AssemblerFullError;

impl fmt::Display for AssemblerFullError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "AssemblerFullError")
    }
}

#[cfg(feature = "std")]
impl std::error::Error for AssemblerFullError {}

/// Holds different fragments of one packet, used for assembling fragmented packets.
///
/// The buffer used for the `PacketAssembler` should either be dynamically sized (ex: Vec<u8>)
/// or should be statically allocated based upon the MTU of the type of packet being
/// assembled (ex: 1280 for a IPv6 frame).
#[derive(Debug)]
pub struct PacketAssembler<K> {
    key: Option<K>,
    buffer: Buffer,

    assembler: Assembler,
    total_size: Option<usize>,
    expires_at: Instant,
}

impl<K> PacketAssembler<K> {
    /// 构造空元数据；实际槽位使用 try_new 准备后备。
    const fn empty() -> Self {
        Self {
            key: None,

            #[cfg(feature = "alloc")]
            buffer: Buffer::new(),
            #[cfg(not(feature = "alloc"))]
            buffer: [0u8; REASSEMBLY_BUFFER_SIZE],

            assembler: Assembler::new(),
            total_size: None,
            expires_at: Instant::ZERO,
        }
    }

    /// 在接口构造期准备槽位，报文处理只访问已初始化的长度。
    pub fn try_new(capacity: usize) -> Result<Self, BufferError> {
        let mut assembler = Self::empty();
        #[cfg(feature = "alloc")]
        {
            assembler.buffer = prepare_buffer(capacity)?;
        }
        #[cfg(not(feature = "alloc"))]
        if capacity != REASSEMBLY_BUFFER_SIZE {
            return Err(BufferError::InvalidCapacity);
        }
        Ok(assembler)
    }

    /// 清除重组状态并保留已准备的 payload 后备。
    pub fn reset(&mut self) {
        self.key = None;
        self.assembler.clear();
        self.total_size = None;
        self.expires_at = Instant::ZERO;
    }

    /// 设置已知总长；超出预备容量时返回错误，不在处理期分配。
    pub fn set_total_size(&mut self, size: usize) -> Result<(), AssemblerError> {
        if let Some(old_size) = self.total_size {
            if old_size != size {
                return Err(AssemblerError);
            }
        }

        if self.buffer.len() < size {
            return Err(AssemblerError);
        }

        self.total_size = Some(size);
        Ok(())
    }

    /// Return the instant when the assembler expires.
    pub fn expires_at(&self) -> Instant {
        self.expires_at
    }

    /// 原地写入后将实际长度登记到原 assembler；段表满明确拒绝。
    pub fn add_with(
        &mut self,
        offset: usize,
        f: impl Fn(&mut [u8]) -> Result<usize, AssemblerError>,
    ) -> Result<(), AssemblerError> {
        if self.buffer.len() < offset {
            return Err(AssemblerError);
        }

        let len = f(&mut self.buffer[offset..])?;
        assert!(offset + len <= self.buffer.len());

        net_debug!(
            "frag assembler: receiving {} octets at offset {}",
            len,
            offset
        );

        self.assembler
            .add(offset, len)
            .map_err(|_| AssemblerError)?;
        Ok(())
    }

    /// 将片段写入预备工作区；偏移溢出和超容量均拒绝。
    pub fn add(&mut self, data: &[u8], offset: usize) -> Result<(), AssemblerError> {
        let end = offset.checked_add(data.len()).ok_or(AssemblerError)?;
        if self.buffer.len() < end {
            return Err(AssemblerError);
        }
        let len = data.len();
        self.buffer[offset..][..len].copy_from_slice(data);

        net_debug!(
            "frag assembler: receiving {} octets at offset {}",
            len,
            offset
        );

        self.assembler
            .add(offset, data.len())
            .map_err(|_| AssemblerError)?;
        Ok(())
    }

    /// Get an immutable slice of the underlying packet data, if reassembly complete.
    /// This will mark the assembler as empty, so that it can be reused.
    pub fn assemble(&mut self) -> Option<&'_ [u8]> {
        if !self.is_complete() {
            return None;
        }

        // NOTE: we can unwrap because `is_complete` already checks this.
        let total_size = self.total_size.unwrap();
        self.reset();
        Some(&self.buffer[..total_size])
    }

    /// Returns `true` when all fragments have been received, otherwise `false`.
    pub fn is_complete(&self) -> bool {
        self.total_size == Some(self.assembler.peek_front())
    }

    /// Returns `true` when the packet assembler is free to use.
    fn is_free(&self) -> bool {
        self.key.is_none()
    }
}

/// Set holding multiple [`PacketAssembler`].
#[derive(Debug)]
pub struct PacketAssemblerSet<K: Eq + Copy> {
    assemblers: [PacketAssembler<K>; REASSEMBLY_BUFFER_COUNT],
}

impl<K: Eq + Copy> PacketAssemblerSet<K> {
    const NEW_PA: PacketAssembler<K> = PacketAssembler::empty();

    /// 在接口构造期逐槽准备有限重组后备，失败时归还已取得槽位。
    pub fn try_new(capacity: usize) -> Result<Self, BufferError> {
        let mut set = Self {
            assemblers: [Self::NEW_PA; REASSEMBLY_BUFFER_COUNT],
        };
        for slot in &mut set.assemblers {
            *slot = PacketAssembler::try_new(capacity)?;
        }
        Ok(set)
    }

    /// Get a [`PacketAssembler`] for a specific key.
    ///
    /// If it doesn't exist, it is created, with the `expires_at` timestamp.
    ///
    /// If the assembler set is full, in which case an error is returned.
    pub fn get(
        &mut self,
        key: &K,
        expires_at: Instant,
    ) -> Result<&mut PacketAssembler<K>, AssemblerFullError> {
        let mut empty_slot = None;
        for slot in &mut self.assemblers {
            if slot.key.as_ref() == Some(key) {
                return Ok(slot);
            }
            if slot.is_free() {
                empty_slot = Some(slot)
            }
        }

        let slot = empty_slot.ok_or(AssemblerFullError)?;
        slot.key = Some(*key);
        slot.expires_at = expires_at;
        Ok(slot)
    }

    /// 在真实最早到期时清除占用，保留各槽的后备。
    pub fn remove_expired(&mut self, timestamp: Instant) {
        for frag in &mut self.assemblers {
            if !frag.is_free() && frag.expires_at <= timestamp {
                frag.reset();
            }
        }
    }

    /// 只报告仍持有重组责任的槽位的最早期限。
    pub fn deadline(&self) -> Option<Instant> {
        self.assemblers
            .iter()
            .filter(|slot| !slot.is_free())
            .map(|slot| slot.expires_at)
            .min()
    }
}

// Max len of non-fragmented packets after decompression (including ipv6 header and payload)
// TODO: lower. Should be (6lowpan mtu) - (min 6lowpan header size) + (max ipv6 header size)
pub const MAX_DECOMPRESSED_LEN: usize = 1500;

#[cfg(feature = "_proto-fragmentation")]
#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum FragKey {
    #[cfg(feature = "proto-ipv4-fragmentation")]
    Ipv4(Ipv4FragKey),
    #[cfg(feature = "proto-sixlowpan-fragmentation")]
    Sixlowpan(SixlowpanFragKey),
}

pub struct FragmentsBuffer {
    #[cfg(feature = "proto-sixlowpan")]
    pub decompress_buf: [u8; MAX_DECOMPRESSED_LEN],

    #[cfg(feature = "_proto-fragmentation")]
    pub assembler: PacketAssemblerSet<FragKey>,

    #[cfg(feature = "_proto-fragmentation")]
    pub reassembly_timeout: Duration,
}

#[cfg(not(feature = "_proto-fragmentation"))]
pub struct Fragmenter {}

#[cfg(not(feature = "_proto-fragmentation"))]
impl Fragmenter {
    /// 未启用分片时不创建 payload 工作区。
    pub fn try_new(_capacity: usize) -> Result<Self, BufferError> {
        Ok(Self {})
    }
}

#[cfg(feature = "_proto-fragmentation")]
pub struct Fragmenter {
    /// 完整未分片包的预备后备。
    #[cfg(feature = "alloc")]
    pub buffer: alloc::vec::Vec<u8>,
    #[cfg(not(feature = "alloc"))]
    pub buffer: [u8; FRAGMENTATION_BUFFER_SIZE],
    /// The size of the packet without the IEEE802.15.4 header and the fragmentation headers.
    pub packet_len: usize,
    /// The amount of bytes that already have been transmitted.
    pub sent_bytes: usize,
    /// 同一完整消息的设备 metadata，尾片沿原值发送。
    pub meta: crate::phy::PacketMeta,
    /// 压缩尾片仍保留其对应的真实 IP 目的。
    pub destination: Option<IpAddress>,

    #[cfg(feature = "proto-ipv4-fragmentation")]
    pub ipv4: Ipv4Fragmenter,
    #[cfg(feature = "proto-sixlowpan-fragmentation")]
    pub sixlowpan: SixlowpanFragmenter,
}

#[cfg(feature = "proto-ipv4-fragmentation")]
pub struct Ipv4Fragmenter {
    /// The IPv4 representation.
    pub repr: Ipv4Repr,
    /// The destination hardware address.
    #[cfg(feature = "medium-ethernet")]
    pub dst_hardware_addr: EthernetAddress,
    /// The offset of the next fragment.
    pub frag_offset: u16,
    /// The identifier of the stream.
    pub ident: u16,
}

#[cfg(feature = "proto-sixlowpan-fragmentation")]
pub struct SixlowpanFragmenter {
    /// The datagram size that is used for the fragmentation headers.
    pub datagram_size: u16,
    /// The datagram tag that is used for the fragmentation headers.
    pub datagram_tag: u16,
    pub datagram_offset: usize,

    /// The size of the FRAG_N packets.
    pub fragn_size: usize,

    /// The link layer IEEE802.15.4 source address.
    pub ll_dst_addr: Ieee802154Address,
    /// The link layer IEEE802.15.4 source address.
    pub ll_src_addr: Ieee802154Address,
}

#[cfg(feature = "_proto-fragmentation")]
impl Fragmenter {
    /// 在接口构造期准备发送后备；传输时只改元数据和已有字节。
    pub fn try_new(capacity: usize) -> Result<Self, BufferError> {
        #[cfg(not(feature = "alloc"))]
        if capacity != FRAGMENTATION_BUFFER_SIZE {
            return Err(BufferError::InvalidCapacity);
        }
        Ok(Self {
            #[cfg(feature = "alloc")]
            buffer: prepare_buffer(capacity)?,
            #[cfg(not(feature = "alloc"))]
            buffer: [0u8; FRAGMENTATION_BUFFER_SIZE],
            packet_len: 0,
            sent_bytes: 0,
            meta: crate::phy::PacketMeta::default(),
            destination: None,

            #[cfg(feature = "proto-ipv4-fragmentation")]
            ipv4: Ipv4Fragmenter {
                repr: Ipv4Repr {
                    src_addr: Ipv4Address::new(0, 0, 0, 0),
                    dst_addr: Ipv4Address::new(0, 0, 0, 0),
                    next_header: IpProtocol::Unknown(0),
                    payload_len: 0,
                    hop_limit: 0,
                },
                #[cfg(feature = "medium-ethernet")]
                dst_hardware_addr: EthernetAddress::default(),
                frag_offset: 0,
                ident: 0,
            },

            #[cfg(feature = "proto-sixlowpan-fragmentation")]
            sixlowpan: SixlowpanFragmenter {
                datagram_size: 0,
                datagram_tag: 0,
                datagram_offset: 0,
                fragn_size: 0,
                ll_dst_addr: Ieee802154Address::Absent,
                ll_src_addr: Ieee802154Address::Absent,
            },
        })
    }

    /// Return `true` when everything is transmitted.
    #[inline]
    pub fn finished(&self) -> bool {
        self.packet_len == self.sent_bytes
    }

    /// Returns `true` when there is nothing to transmit.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.packet_len == 0
    }

    /// 清除发送责任并保留完整 payload 后备。
    pub fn reset(&mut self) {
        self.packet_len = 0;
        self.sent_bytes = 0;
        self.meta = crate::phy::PacketMeta::default();
        self.destination = None;

        #[cfg(feature = "proto-ipv4-fragmentation")]
        {
            self.ipv4.repr = Ipv4Repr {
                src_addr: Ipv4Address::new(0, 0, 0, 0),
                dst_addr: Ipv4Address::new(0, 0, 0, 0),
                next_header: IpProtocol::Unknown(0),
                payload_len: 0,
                hop_limit: 0,
            };
            #[cfg(feature = "medium-ethernet")]
            {
                self.ipv4.dst_hardware_addr = EthernetAddress::default();
            }
        }

        #[cfg(feature = "proto-sixlowpan-fragmentation")]
        {
            self.sixlowpan.datagram_size = 0;
            self.sixlowpan.datagram_tag = 0;
            self.sixlowpan.fragn_size = 0;
            self.sixlowpan.ll_dst_addr = Ieee802154Address::Absent;
            self.sixlowpan.ll_src_addr = Ieee802154Address::Absent;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
    struct Key {
        id: usize,
    }

    #[test]
    /// 原重叠片段只在预备后备中覆盖相应字节。
    fn packet_assembler_overlap() {
        let mut p_assembler = PacketAssembler::<Key>::try_new(REASSEMBLY_BUFFER_SIZE).unwrap();

        p_assembler.set_total_size(5).unwrap();

        let data = b"Rust";
        p_assembler.add(&data[..], 0).unwrap();
        p_assembler.add(&data[..], 1).unwrap();

        assert_eq!(p_assembler.assemble(), Some(&b"RRust"[..]))
    }

    #[test]
    /// 预备后备中的原顺序重组仍交出完整 payload。
    fn packet_assembler_assemble() {
        let mut p_assembler = PacketAssembler::<Key>::try_new(REASSEMBLY_BUFFER_SIZE).unwrap();

        let data = b"Hello World!";

        p_assembler.set_total_size(data.len()).unwrap();

        p_assembler.add(b"Hello ", 0).unwrap();
        assert_eq!(p_assembler.assemble(), None);

        p_assembler.add(b"World!", b"Hello ".len()).unwrap();

        assert_eq!(p_assembler.assemble(), Some(&b"Hello World!"[..]));
    }

    #[test]
    /// 预备后备中的乱序重组保留原洞表算法。
    fn packet_assembler_out_of_order_assemble() {
        let mut p_assembler = PacketAssembler::<Key>::try_new(REASSEMBLY_BUFFER_SIZE).unwrap();

        let data = b"Hello World!";

        p_assembler.set_total_size(data.len()).unwrap();

        p_assembler.add(b"World!", b"Hello ".len()).unwrap();
        assert_eq!(p_assembler.assemble(), None);

        p_assembler.add(b"Hello ", 0).unwrap();

        assert_eq!(p_assembler.assemble(), Some(&b"Hello World!"[..]));
    }

    #[test]
    /// 预备后的槽位可为真实 key 取得重组责任。
    fn packet_assembler_set() {
        let key = Key { id: 1 };

        let mut set = PacketAssemblerSet::try_new(REASSEMBLY_BUFFER_SIZE).unwrap();

        assert!(set.get(&key, Instant::ZERO).is_ok());
    }

    #[test]
    /// 原固定槽位全部占用后明确拒绝新 key。
    fn packet_assembler_set_full() {
        let mut set = PacketAssemblerSet::try_new(REASSEMBLY_BUFFER_SIZE).unwrap();
        for i in 0..REASSEMBLY_BUFFER_COUNT {
            set.get(&Key { id: i }, Instant::ZERO).unwrap();
        }
        assert!(set.get(&Key { id: 4 }, Instant::ZERO).is_err());
    }

    #[test]
    /// 完成重组后同一预备槽位立即可供后继 key 使用。
    fn packet_assembler_set_assembling_many() {
        let mut set = PacketAssemblerSet::try_new(REASSEMBLY_BUFFER_SIZE).unwrap();

        let key = Key { id: 0 };
        let assr = set.get(&key, Instant::ZERO).unwrap();
        assert_eq!(assr.assemble(), None);
        assr.set_total_size(0).unwrap();
        assr.assemble().unwrap();

        // Test that `.assemble()` effectively deletes it.
        let assr = set.get(&key, Instant::ZERO).unwrap();
        assert_eq!(assr.assemble(), None);
        assr.set_total_size(0).unwrap();
        assr.assemble().unwrap();

        let key = Key { id: 1 };
        let assr = set.get(&key, Instant::ZERO).unwrap();
        assr.set_total_size(0).unwrap();
        assr.assemble().unwrap();

        let key = Key { id: 2 };
        let assr = set.get(&key, Instant::ZERO).unwrap();
        assr.set_total_size(0).unwrap();
        assr.assemble().unwrap();

        let key = Key { id: 2 };
        let assr = set.get(&key, Instant::ZERO).unwrap();
        assr.set_total_size(2).unwrap();
        assr.add(&[0x00], 0).unwrap();
        assert_eq!(assr.assemble(), None);
        let assr = set.get(&key, Instant::ZERO).unwrap();
        assr.add(&[0x01], 1).unwrap();
        assert_eq!(assr.assemble(), Some(&[0x00, 0x01][..]));
    }

    #[test]
    /// 超容量和偏移溢出不扩后备，reset 后原槽仍可重用。
    fn packet_assembler_capacity_and_expiry() {
        let mut assembler = PacketAssembler::<Key>::try_new(REASSEMBLY_BUFFER_SIZE).unwrap();
        assert_eq!(
            assembler.set_total_size(REASSEMBLY_BUFFER_SIZE + 1),
            Err(AssemblerError)
        );
        assert_eq!(assembler.add(&[1], usize::MAX), Err(AssemblerError));
        assembler.set_total_size(2).unwrap();
        assembler.add(&[7, 9], 0).unwrap();
        assert_eq!(assembler.assemble(), Some(&[7, 9][..]));
        let mut set = PacketAssemblerSet::try_new(REASSEMBLY_BUFFER_SIZE).unwrap();
        let deadline = Instant::from_millis(42);
        set.get(&Key { id: 1 }, deadline)
            .unwrap()
            .add(&[5], 0)
            .unwrap();
        assert_eq!(set.deadline(), Some(deadline));
        set.remove_expired(deadline);
        assert_eq!(set.deadline(), None);
        set.get(&Key { id: 2 }, deadline)
            .unwrap()
            .set_total_size(1)
            .unwrap();
    }
}
