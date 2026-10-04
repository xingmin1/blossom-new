//! 纯协议接口的跨 crate 注册证据；不建立 HAL、设备、堆或文件系统资源。

use crate::device::blk::decode_status;
use crate::queue::RingLayout;
use crate::{DmaAddr, Error};

/// 显式引用所有本叶 suite 的真实槽；归档抽取与 GC/LTO 保留仍由最终 ELF 实证。
pub fn anchors() -> [&'static ::ktest::SuiteSlot; 3] {
    [
        ring::__ktest_anchor(),
        status::__ktest_anchor(),
        address::__ktest_anchor(),
    ]
}

#[::ktest::suite(
    context = "Boot",
    tags("selftest", "selftest-normal", "virtio", "noheap")
)]
mod ring {
    use super::*;

    /// 固定协议布局边界直接调用生产 RingLayout，不复制布局计算算法。
    #[test]
    #[case("modern_min", 4u16, false, 64usize, 80usize, 118usize)]
    #[case("modern_eight", 8u16, false, 128usize, 152usize, 222usize)]
    #[case("modern_tags", 32u16, false, 512usize, 584usize, 846usize)]
    #[case("modern_max", 128u16, false, 2048usize, 2312usize, 3342usize)]
    #[case("legacy_min", 4u16, true, 64usize, 4096usize, 4134usize)]
    #[case("legacy_max", 128u16, true, 2048usize, 4096usize, 5126usize)]
    fn boundary(depth: u16, legacy: bool, avail: usize, used: usize, bytes: usize) {
        let layout = RingLayout::new(depth, legacy).unwrap();
        assert_eq!(layout.depth(), depth);
        assert_eq!(layout.avail_offset(), avail);
        assert_eq!(layout.used_offset(), used);
        assert_eq!(layout.byte_len(), bytes);
    }

    /// 真实深度限制拒绝零、小于最小、非二次幂和越过最大值。
    #[test]
    #[case("zero", 0u16)]
    #[case("below_min", 2u16)]
    #[case("non_power", 5u16)]
    #[case("below_max_non_power", 127u16)]
    #[case("above_max", 256u16)]
    #[case("u16_max", u16::MAX)]
    fn rejected(depth: u16) {
        for legacy in [false, true] {
            assert!(matches!(
                RingLayout::new(depth, legacy),
                Err(Error::InvalidParam)
            ));
        }
    }
}

#[::ktest::suite(
    context = "Boot",
    tags("selftest", "selftest-normal", "virtio", "noheap")
)]
mod status {
    use super::*;

    /// 相同短 case 名位于另一个 suite，完整目录身份必须不同。
    #[test]
    #[case("success", 0u8, None)]
    #[case("io_error", 1u8, Some(Error::IoError))]
    #[case("unsupported", 2u8, Some(Error::Unsupported))]
    #[case("undefined", 3u8, Some(Error::IoError))]
    #[case("high_undefined", 0x7fu8, Some(Error::IoError))]
    #[case("unfinished", 0xffu8, Some(Error::IoError))]
    fn boundary(raw: u8, error: Option<Error>) {
        assert_eq!(decode_status(raw), error.map_or(Ok(()), Err));
    }

    /// 对所有可能的设备写入字节检查只有协议零值可完成为成功。
    #[test]
    fn only_zero_succeeds() {
        for raw in u8::MIN..=u8::MAX {
            assert_eq!(decode_status(raw).is_ok(), raw == 0);
        }
    }
}

#[::ktest::suite(
    context = "Boot",
    tags("selftest", "selftest-normal", "virtio", "noheap")
)]
mod address {
    use super::*;

    /// 真实 DMA 地址域的上界检查不创建假 CPU 指针或假 DMA 映射。
    #[test]
    #[case("zero", 0u64, 0usize, Some(0u64))]
    #[case("last_byte", u64::MAX - 1, 1usize, Some(u64::MAX))]
    #[case("max_zero", u64::MAX, 0usize, Some(u64::MAX))]
    #[case("overflow", u64::MAX, 1usize, None)]
    fn boundary(base: u64, offset: usize, expected: Option<u64>) {
        assert_eq!(
            DmaAddr::new(base).checked_add(offset).map(DmaAddr::value),
            expected
        );
    }
}
