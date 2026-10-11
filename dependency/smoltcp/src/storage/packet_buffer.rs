use managed::ManagedSlice;

use crate::storage::{Empty, Full, RingBuffer};

/// Size and header of a packet.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct PacketMetadata<H> {
    size: usize,
    header: Option<H>,
}

impl<H> PacketMetadata<H> {
    /// Empty packet description.
    pub const EMPTY: PacketMetadata<H> = PacketMetadata {
        size: 0,
        header: None,
    };

    fn padding(size: usize) -> PacketMetadata<H> {
        PacketMetadata {
            size: size,
            header: None,
        }
    }

    fn packet(size: usize, header: H) -> PacketMetadata<H> {
        PacketMetadata {
            size: size,
            header: Some(header),
        }
    }

    fn is_padding(&self) -> bool {
        self.header.is_none()
    }
}

/// An UDP packet ring buffer.
#[derive(Debug)]
pub struct PacketBuffer<'a, H: 'a> {
    metadata_ring: RingBuffer<'a, PacketMetadata<H>>,
    payload_ring: RingBuffer<'a, u8>,
}

impl<'a, H> PacketBuffer<'a, H> {
    /// Create a new packet buffer with the provided metadata and payload storage.
    ///
    /// Metadata storage limits the maximum _number_ of packets in the buffer and payload
    /// storage limits the maximum _total size_ of packets.
    pub fn new<MS, PS>(metadata_storage: MS, payload_storage: PS) -> PacketBuffer<'a, H>
    where
        MS: Into<ManagedSlice<'a, PacketMetadata<H>>>,
        PS: Into<ManagedSlice<'a, u8>>,
    {
        PacketBuffer {
            metadata_ring: RingBuffer::new(metadata_storage),
            payload_ring: RingBuffer::new(payload_storage),
        }
    }

    /// Query whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.metadata_ring.is_empty()
    }

    /// Query whether the buffer is full.
    pub fn is_full(&self) -> bool {
        self.metadata_ring.is_full()
    }

    /// 查询接纳完整消息所需的 payload 字节，包含绕回 padding；metadata 或连续空间不足时返回 None。
    pub fn enqueue_payload_cost(&self, size: usize) -> Option<usize> {
        let window = self.payload_ring.window();
        let contiguous = if self.payload_ring.is_empty() {
            self.payload_ring.capacity()
        } else {
            self.payload_ring.contiguous_window()
        };
        if window < size {
            return None;
        }
        let padding = if contiguous < size { contiguous } else { 0 };
        if window - padding < size {
            return None;
        }
        let records = if padding == 0 { 1 } else { 2 };
        if self.metadata_ring.window() < records {
            return None;
        }
        Some(padding + size)
    }

    // There is currently no enqueue_with() because of the complexity of managing padding
    // in case of failure.

    /// 接纳一条完整消息并借出 payload；不足时保持 metadata 和 payload 两个环不变。
    pub fn enqueue(&mut self, size: usize, header: H) -> Result<&mut [u8], Full> {
        let padding = self.enqueue_payload_cost(size).ok_or(Full)? - size;
        if self.payload_ring.is_empty() {
            self.payload_ring.clear();
        }
        if 0 < padding {
            *self.metadata_ring.enqueue_one()? = PacketMetadata::padding(padding);
            let _ = self.payload_ring.enqueue_many(padding);
        }
        *self.metadata_ring.enqueue_one()? = PacketMetadata::packet(size, header);
        let payload_buf = self.payload_ring.enqueue_many(size);
        debug_assert_eq!(payload_buf.len(), size);
        Ok(payload_buf)
    }

    /// 预留 max_size 的连续空间，再提交闭包实际写入的一条消息；空环先复位以免保留无用 padding。
    pub fn enqueue_with_infallible<'b, F>(
        &'b mut self,
        max_size: usize,
        header: H,
        f: F,
    ) -> Result<usize, Full>
    where
        F: FnOnce(&'b mut [u8]) -> usize,
    {
        let padding = self.enqueue_payload_cost(max_size).ok_or(Full)? - max_size;
        if self.payload_ring.is_empty() {
            self.payload_ring.clear();
        }
        if 0 < padding {
            *self.metadata_ring.enqueue_one()? = PacketMetadata::padding(padding);
            let _ = self.payload_ring.enqueue_many(padding);
        }
        let (size, _) = self.payload_ring.enqueue_many_with(|data| {
            let size = f(&mut data[..max_size]);
            assert!(size <= max_size);
            (size, ())
        });
        *self.metadata_ring.enqueue_one()? = PacketMetadata::packet(size, header);
        Ok(size)
    }

    fn dequeue_padding(&mut self) {
        let _ = self.metadata_ring.dequeue_one_with(|metadata| {
            if metadata.is_padding() {
                // note(discard): function does not use value of dequeued padding bytes
                let _buf_dequeued = self.payload_ring.dequeue_many(metadata.size);
                Ok(()) // dequeue metadata
            } else {
                Err(()) // don't dequeue metadata
            }
        });
    }

    /// Call `f` with a single packet from the buffer, and dequeue the packet if `f`
    /// returns successfully, or return `Err(EmptyError)` if the buffer is empty.
    pub fn dequeue_with<'c, R, E, F>(&'c mut self, f: F) -> Result<Result<R, E>, Empty>
    where
        F: FnOnce(&mut H, &'c mut [u8]) -> Result<R, E>,
    {
        self.dequeue_padding();

        self.metadata_ring.dequeue_one_with(|metadata| {
            self.payload_ring
                .dequeue_many_with(|payload_buf| {
                    debug_assert!(payload_buf.len() >= metadata.size);

                    match f(
                        metadata.header.as_mut().unwrap(),
                        &mut payload_buf[..metadata.size],
                    ) {
                        Ok(val) => (metadata.size, Ok(val)),
                        Err(err) => (0, Err(err)),
                    }
                })
                .1
        })
    }

    /// Dequeue a single packet from the buffer, and return a reference to its payload
    /// as well as its header, or return `Err(Error::Exhausted)` if the buffer is empty.
    pub fn dequeue(&mut self) -> Result<(H, &mut [u8]), Empty> {
        self.dequeue_padding();

        let meta = self.metadata_ring.dequeue_one()?;

        let payload_buf = self.payload_ring.dequeue_many(meta.size);
        debug_assert!(payload_buf.len() == meta.size);
        Ok((meta.header.take().unwrap(), payload_buf))
    }

    /// Peek at a single packet from the buffer without removing it, and return a reference to
    /// its payload as well as its header, or return `Err(Error:Exhausted)` if the buffer is empty.
    ///
    /// This function otherwise behaves identically to [dequeue](#method.dequeue).
    pub fn peek(&mut self) -> Result<(&H, &[u8]), Empty> {
        self.dequeue_padding();

        if let Some(metadata) = self.metadata_ring.get_allocated(0, 1).first() {
            Ok((
                metadata.header.as_ref().unwrap(),
                self.payload_ring.get_allocated(0, metadata.size),
            ))
        } else {
            Err(Empty)
        }
    }

    /// Return the maximum number packets that can be stored.
    pub fn packet_capacity(&self) -> usize {
        self.metadata_ring.capacity()
    }

    /// Return the maximum number of bytes in the payload ring buffer.
    pub fn payload_capacity(&self) -> usize {
        self.payload_ring.capacity()
    }

    /// Return the current number of bytes in the payload ring buffer.
    pub fn payload_bytes_count(&self) -> usize {
        self.payload_ring.len()
    }

    /// Reset the packet buffer and clear any staged.
    #[allow(unused)]
    pub fn reset(&mut self) {
        self.payload_ring.clear();
        self.metadata_ring.clear();
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn buffer() -> PacketBuffer<'static, ()> {
        PacketBuffer::new(vec![PacketMetadata::EMPTY; 4], vec![0u8; 16])
    }

    #[test]
    fn test_simple() {
        let mut buffer = buffer();
        buffer.enqueue(6, ()).unwrap().copy_from_slice(b"abcdef");
        assert_eq!(buffer.enqueue(16, ()), Err(Full));
        assert_eq!(buffer.metadata_ring.len(), 1);
        assert_eq!(buffer.dequeue().unwrap().1, &b"abcdef"[..]);
        assert_eq!(buffer.dequeue(), Err(Empty));
    }

    #[test]
    fn test_peek() {
        let mut buffer = buffer();
        assert_eq!(buffer.peek(), Err(Empty));
        buffer.enqueue(6, ()).unwrap().copy_from_slice(b"abcdef");
        assert_eq!(buffer.metadata_ring.len(), 1);
        assert_eq!(buffer.peek().unwrap().1, &b"abcdef"[..]);
        assert_eq!(buffer.dequeue().unwrap().1, &b"abcdef"[..]);
        assert_eq!(buffer.peek(), Err(Empty));
    }

    #[test]
    fn test_padding() {
        let mut buffer = buffer();
        assert!(buffer.enqueue(6, ()).is_ok());
        assert!(buffer.enqueue(8, ()).is_ok());
        assert!(buffer.dequeue().is_ok());
        buffer.enqueue(4, ()).unwrap().copy_from_slice(b"abcd");
        assert_eq!(buffer.metadata_ring.len(), 3);
        assert!(buffer.dequeue().is_ok());

        assert_eq!(buffer.dequeue().unwrap().1, &b"abcd"[..]);
        assert_eq!(buffer.metadata_ring.len(), 0);
    }

    #[test]
    fn test_padding_with_large_payload() {
        let mut buffer = buffer();
        assert!(buffer.enqueue(12, ()).is_ok());
        assert!(buffer.dequeue().is_ok());
        buffer
            .enqueue(12, ())
            .unwrap()
            .copy_from_slice(b"abcdefghijkl");
    }

    #[test]
    fn test_dequeue_with() {
        let mut buffer = buffer();
        assert!(buffer.enqueue(6, ()).is_ok());
        assert!(buffer.enqueue(8, ()).is_ok());
        assert!(buffer.dequeue().is_ok());
        buffer.enqueue(4, ()).unwrap().copy_from_slice(b"abcd");
        assert_eq!(buffer.metadata_ring.len(), 3);
        assert!(buffer.dequeue().is_ok());

        assert!(matches!(
            buffer.dequeue_with(|_, _| Result::<(), u32>::Err(123)),
            Ok(Err(_))
        ));
        assert_eq!(buffer.metadata_ring.len(), 1);

        assert!(buffer
            .dequeue_with(|&mut (), payload| {
                assert_eq!(payload, &b"abcd"[..]);
                Result::<(), ()>::Ok(())
            })
            .is_ok());
        assert_eq!(buffer.metadata_ring.len(), 0);
    }

    #[test]
    fn test_metadata_full_empty() {
        let mut buffer = buffer();
        assert!(buffer.is_empty());
        assert!(!buffer.is_full());
        assert!(buffer.enqueue(1, ()).is_ok());
        assert!(!buffer.is_empty());
        assert!(buffer.enqueue(1, ()).is_ok());
        assert!(buffer.enqueue(1, ()).is_ok());
        assert!(!buffer.is_full());
        assert!(!buffer.is_empty());
        assert!(buffer.enqueue(1, ()).is_ok());
        assert!(buffer.is_full());
        assert!(!buffer.is_empty());
        assert_eq!(buffer.metadata_ring.len(), 4);
        assert_eq!(buffer.enqueue(1, ()), Err(Full));
    }

    #[test]
    fn test_window_too_small() {
        let mut buffer = buffer();
        assert!(buffer.enqueue(4, ()).is_ok());
        assert!(buffer.enqueue(8, ()).is_ok());
        assert!(buffer.dequeue().is_ok());
        assert_eq!(buffer.enqueue(16, ()), Err(Full));
        assert_eq!(buffer.metadata_ring.len(), 1);
    }

    #[test]
    fn test_contiguous_window_too_small() {
        let mut buffer = buffer();
        assert!(buffer.enqueue(4, ()).is_ok());
        assert!(buffer.enqueue(8, ()).is_ok());
        assert!(buffer.dequeue().is_ok());
        assert_eq!(buffer.enqueue(8, ()), Err(Full));
        assert_eq!(buffer.metadata_ring.len(), 1);
    }

    #[test]
    fn test_contiguous_window_wrap() {
        let mut buffer = buffer();
        assert!(buffer.enqueue(15, ()).is_ok());
        assert!(buffer.dequeue().is_ok());
        assert!(buffer.enqueue(16, ()).is_ok());
    }

    #[test]
    fn test_capacity_too_small() {
        let mut buffer = buffer();
        assert_eq!(buffer.enqueue(32, ()), Err(Full));
    }

    #[test]
    fn test_contig_window_prioritized() {
        let mut buffer = buffer();
        assert!(buffer.enqueue(4, ()).is_ok());
        assert!(buffer.dequeue().is_ok());
        assert!(buffer.enqueue(5, ()).is_ok());
    }

    /// 验证绕回需要两个 metadata，失败不得留下孤立 padding 或改变既有 payload。
    #[test]
    fn test_padding_requires_metadata_reservation() {
        let mut buffer = PacketBuffer::new(vec![PacketMetadata::EMPTY; 2], vec![0; 16]);
        buffer.enqueue(6, ()).unwrap().copy_from_slice(b"abcdef");
        buffer.enqueue(8, ()).unwrap().copy_from_slice(b"12345678");
        assert_eq!(buffer.dequeue().unwrap().1, b"abcdef");
        assert_eq!(buffer.enqueue_payload_cost(4), None);
        assert_eq!(buffer.enqueue(4, ()), Err(Full));
        assert_eq!(
            buffer.enqueue_with_infallible(4, (), |_| unreachable!()),
            Err(Full)
        );
        assert_eq!(buffer.dequeue().unwrap().1, b"12345678");
        assert!(buffer.is_empty());
        assert_eq!(buffer.payload_bytes_count(), 0);
    }

    /// 验证空环 send_with 使用整块容量，实际缩短后仅保存一个完整记录。
    #[test]
    fn test_enqueue_with_reuses_empty_ring() {
        let mut buffer = buffer();
        buffer
            .enqueue(12, ())
            .unwrap()
            .copy_from_slice(b"abcdefghijkl");
        assert_eq!(buffer.dequeue().unwrap().1, b"abcdefghijkl");
        assert_eq!(buffer.enqueue_payload_cost(16), Some(16));
        assert_eq!(
            buffer.enqueue_with_infallible(16, (), |bytes| {
                bytes[..6].copy_from_slice(b"abcdef");
                6
            }),
            Ok(6)
        );
        assert_eq!(buffer.dequeue().unwrap().1, b"abcdef");
        assert!(buffer.is_empty());
    }

    #[test]
    fn clear() {
        let mut buffer = buffer();

        // Ensure enqueuing data in the buffer fills it somewhat.
        assert!(buffer.is_empty());
        assert!(buffer.enqueue(6, ()).is_ok());

        // Ensure that resetting the buffer causes it to be empty.
        assert!(!buffer.is_empty());
        buffer.reset();
        assert!(buffer.is_empty());
    }
}
