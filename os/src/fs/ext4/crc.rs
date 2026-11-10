//! ext4 元数据校验和算法（CRC32C 与 CRC16）。
//!
//! # 模块职责与定位
//! 本模块位于 `fs::ext4` 底层，为 `codec` 和 `allocation` 提供纯计算的字节流循环冗余校验（CRC），
//! 不涉及任何锁、I/O 或缓存状态。
//!
//! # 核心设计
//! - **增量式 CRC32C（Castagnoli，反射多项式 `0x82F63B78`）**：
//!   ext4 启用 `METADATA_CSUM` 时，元数据校验和不是对单一连续缓冲区独立做完整 CRC32C，
//!   而是先以文件系统 `uuid`（或超级块预存的 `s_checksum_seed`）计算基础种子，再把 inode 编号、
//!   代数（generation）以及分段元数据字节依次串接进同一条校验链。因此 [`crc32c`] 保留未最终取反的
//!   中间寄存器值，由调用方以 `u32::MAX`（`~0`）初始化并在链式计算中直接传递 `seed`。
//! - **反射 CRC16（多项式 `0x8005`，反射值 `0xA001`）**：
//!   供仅启用旧式 `GDT_CSUM`（未启用 `METADATA_CSUM`）的卷计算块组描述符校验和。

/// 按 16 字节分片计算 Castagnoli CRC32C，查找表在编译期生成。
static CRC32C: crc::Crc<u32, crc::Table<16>> =
    crc::Crc::<u32, crc::Table<16>>::new(&crc::CRC_32_ISCSI);

/// 以 `seed` 为当前寄存器初值，增量更新反射 CRC32C 状态并返回未最终取反的结果。
///
/// 无睡眠、无锁纯函数，可在任意上下文调用。ext4 元数据校验链通常以 `u32::MAX` 作为首个种子，
/// 连续灌入 UUID、编号及各段结构字节后直接按 ext4 盘面约定截断或写入对应校验和字段。
pub fn crc32c(seed: u32, bytes: &[u8]) -> u32 {
    // ISCSI 算法初始化时反转输入位序、结束时取反；抵消这两次转换以保留原始寄存器值。
    let mut digest = CRC32C.digest_with_initial(seed.reverse_bits());
    digest.update(bytes);
    !digest.finalize()
}

/// 以 `seed` 为初值逐位计算反射 CRC16（生成多项式 `0x8005`，反射掩码 `0xa001`）。
///
/// 无睡眠、无锁纯函数，用于旧式 `FEATURE_RO_COMPAT_GDT_CSUM` 块组描述符校验和。
pub fn crc16(mut seed: u16, bytes: &[u8]) -> u16 {
    for &byte in bytes {
        seed ^= u16::from(byte);
        for _ in 0..8 {
            seed = (seed >> 1) ^ if seed & 1 != 0 { 0xa001 } else { 0 };
        }
    }
    seed
}
