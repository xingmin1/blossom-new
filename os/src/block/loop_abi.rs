//! Linux loop 旧式控制命令及 `loop_info64` 的固定字节布局。
//!
//! 用户内存只以 232 字节数组复制；编码和解码按明确字段偏移执行，不读取 Rust 填充字节。
//! 本轮只支持整个 backing 文件、诊断文件名和 AUTOCLEAR；只读状态由绑定时的实际文件能力决定。

use core::convert::TryInto;

use crate::fs::{FileStat, FsError};

/// 将当前用户文件描述符绑定为 loop backing 文件。
pub(crate) const LOOP_SET_FD: u32 = 0x4c00;
/// 请求在最后一个真实使用者退出后解除绑定。
pub(crate) const LOOP_CLR_FD: u32 = 0x4c01;
/// 设置 64 位状态结构中本实现支持的字段。
pub(crate) const LOOP_SET_STATUS64: u32 = 0x4c04;
/// 查询 64 位状态结构及 backing 文件身份。
pub(crate) const LOOP_GET_STATUS64: u32 = 0x4c05;
/// backing 文件或绑定时的 loop 打开模式不允许写入。
pub(crate) const LO_FLAGS_READ_ONLY: u32 = 1;
/// 最后一个打开实例和挂载租约退出后自动解除绑定。
pub(crate) const LO_FLAGS_AUTOCLEAR: u32 = 4;
/// Linux UAPI 中 `struct loop_info64` 的固定字节数。
pub(crate) const LOOP_INFO64_BYTES: usize = 232;
/// Linux UAPI 中诊断文件名的固定字节数，包含末尾 NUL。
pub(crate) const LO_NAME_SIZE: usize = 64;

/// `loop_info64` 的字段值；用户 ABI 由显式编码方法维护。
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct LoopInfo64 {
    /// backing 文件的文件系统设备号，只供查询。
    pub device: u64,
    /// backing 文件在其文件系统中的 inode 号，只供查询。
    pub inode: u64,
    /// backing 文件的特殊设备号；普通文件为零，只供查询。
    pub rdevice: u64,
    /// backing 文件中的起始字节偏移，本实现只接受零。
    pub offset: u64,
    /// 暴露窗口的字节上限，本实现只接受表示整个文件的零。
    pub sizelimit: u64,
    /// loop 设备编号；固定设备为零，只供查询。
    pub number: u32,
    /// 加密算法编号，本实现只接受无加密的零。
    pub encrypt_type: u32,
    /// 加密密钥字节数，本实现只接受零。
    pub encrypt_key_size: u32,
    /// READ_ONLY 与 AUTOCLEAR 状态位。
    pub flags: u32,
    /// 诊断文件名，不参与路径解析或 backing 文件的保活。
    pub file_name: [u8; LO_NAME_SIZE],
    /// 加密算法名称，本实现只接受全零。
    pub crypt_name: [u8; LO_NAME_SIZE],
    /// 加密密钥，本实现只接受全零且查询时始终清零。
    pub encrypt_key: [u8; 32],
    /// 保留的加密初始化字段，本实现只接受全零。
    pub init: [u64; 2],
}

const _: () = {
    assert!(core::mem::size_of::<LoopInfo64>() == LOOP_INFO64_BYTES);
    assert!(core::mem::offset_of!(LoopInfo64, flags) == 52);
    assert!(core::mem::offset_of!(LoopInfo64, file_name) == 56);
    assert!(core::mem::offset_of!(LoopInfo64, crypt_name) == 120);
    assert!(core::mem::offset_of!(LoopInfo64, encrypt_key) == 184);
    assert!(core::mem::offset_of!(LoopInfo64, init) == 216);
};

impl LoopInfo64 {
    /// 由实际 backing 文件属性及当前绑定状态构造查询结果，其余字段全部清零。
    pub(crate) fn status(stat: FileStat, flags: u32, file_name: [u8; LO_NAME_SIZE]) -> Self {
        Self {
            device: stat.device,
            inode: stat.inode,
            rdevice: stat.rdev,
            offset: 0,
            sizelimit: 0,
            number: 0,
            encrypt_type: 0,
            encrypt_key_size: 0,
            flags,
            file_name,
            crypt_name: [0; LO_NAME_SIZE],
            encrypt_key: [0; 32],
            init: [0; 2],
        }
    }

    /// 解码一次已经完整复制到内核的用户结构，不借用用户地址或持有设备锁。
    pub(crate) fn from_bytes(bytes: &[u8; LOOP_INFO64_BYTES]) -> Self {
        Self {
            device: read_u64(bytes, 0),
            inode: read_u64(bytes, 8),
            rdevice: read_u64(bytes, 16),
            offset: read_u64(bytes, 24),
            sizelimit: read_u64(bytes, 32),
            number: read_u32(bytes, 40),
            encrypt_type: read_u32(bytes, 44),
            encrypt_key_size: read_u32(bytes, 48),
            flags: read_u32(bytes, 52),
            file_name: bytes[56..120].try_into().expect("loop file name size"),
            crypt_name: bytes[120..184].try_into().expect("loop crypt name size"),
            encrypt_key: bytes[184..216].try_into().expect("loop key size"),
            init: [read_u64(bytes, 216), read_u64(bytes, 224)],
        }
    }

    /// 按本机 Linux UAPI 字节序编码全部字段，结果没有未初始化字节。
    pub(crate) fn to_bytes(self) -> [u8; LOOP_INFO64_BYTES] {
        let mut bytes = [0; LOOP_INFO64_BYTES];
        bytes[0..8].copy_from_slice(&self.device.to_ne_bytes());
        bytes[8..16].copy_from_slice(&self.inode.to_ne_bytes());
        bytes[16..24].copy_from_slice(&self.rdevice.to_ne_bytes());
        bytes[24..32].copy_from_slice(&self.offset.to_ne_bytes());
        bytes[32..40].copy_from_slice(&self.sizelimit.to_ne_bytes());
        bytes[40..44].copy_from_slice(&self.number.to_ne_bytes());
        bytes[44..48].copy_from_slice(&self.encrypt_type.to_ne_bytes());
        bytes[48..52].copy_from_slice(&self.encrypt_key_size.to_ne_bytes());
        bytes[52..56].copy_from_slice(&self.flags.to_ne_bytes());
        bytes[56..120].copy_from_slice(&self.file_name);
        bytes[120..184].copy_from_slice(&self.crypt_name);
        bytes[184..216].copy_from_slice(&self.encrypt_key);
        bytes[216..224].copy_from_slice(&self.init[0].to_ne_bytes());
        bytes[224..232].copy_from_slice(&self.init[1].to_ne_bytes());
        bytes
    }

    /// 拒绝非零窗口、分区扫描、direct I/O、未知标志和任何加密请求。
    pub(crate) fn check_supported(&self) -> Result<(), FsError> {
        if self.offset != 0
            || self.sizelimit != 0
            || self.flags & !(LO_FLAGS_READ_ONLY | LO_FLAGS_AUTOCLEAR) != 0
            || self.encrypt_type != 0
            || self.encrypt_key_size != 0
            || self.crypt_name.iter().any(|byte| *byte != 0)
            || self.encrypt_key.iter().any(|byte| *byte != 0)
            || self.init != [0; 2]
        {
            return Err(FsError::Unsupported);
        }
        Ok(())
    }
}

/// 从已完整复制的固定结构中读取一个自然字节序 64 位字段。
fn read_u64(bytes: &[u8; LOOP_INFO64_BYTES], offset: usize) -> u64 {
    u64::from_ne_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("loop u64 field size"),
    )
}

/// 从已完整复制的固定结构中读取一个自然字节序 32 位字段。
fn read_u32(bytes: &[u8; LOOP_INFO64_BYTES], offset: usize) -> u32 {
    u32::from_ne_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("loop u32 field size"),
    )
}

#[cfg(feature = "ktest")]
#[::ktest::suite(context = "Boot", tags("block", "loop-abi"), timeout_ms = 1000)]
mod checks {
    use super::{LoopInfo64, LO_FLAGS_AUTOCLEAR, LO_FLAGS_READ_ONLY};
    use crate::fs::{FileStat, FsError, InodeKind, InodeTime};

    /// 测试目标：验证 `loop_info64` 的字段偏移和独立原始字节图样的解码结果。
    /// 场景步骤：解码各字段均有不同内容的 232 字节结构，并核对结构体布局。
    /// 预期结果：每个字段均来自规定的字节位置，文件名和加密字段的完整内容不丢失。
    #[test]
    fn decode_uapi_layout() {
        let bytes: [u8; 232] = *b"\
DEV12345\
INO12345\
RDEV1234\
OFFSET01\
SIZE1234\
NUM!\
ENC!\
KEY!\
FLAG\
NAME0001NAME0002NAME0003NAME0004NAME0005NAME0006NAME0007NAME0008\
CRYPT001CRYPT002CRYPT003CRYPT004CRYPT005CRYPT006CRYPT007CRYPT008\
KEY_0001KEY_0002KEY_0003KEY_0004\
INIT0001\
INIT0002";
        let info = LoopInfo64::from_bytes(&bytes);

        assert_eq!(info.device, u64::from_ne_bytes(*b"DEV12345"));
        assert_eq!(info.inode, u64::from_ne_bytes(*b"INO12345"));
        assert_eq!(info.rdevice, u64::from_ne_bytes(*b"RDEV1234"));
        assert_eq!(info.offset, u64::from_ne_bytes(*b"OFFSET01"));
        assert_eq!(info.sizelimit, u64::from_ne_bytes(*b"SIZE1234"));
        assert_eq!(info.number, u32::from_ne_bytes(*b"NUM!"));
        assert_eq!(info.encrypt_type, u32::from_ne_bytes(*b"ENC!"));
        assert_eq!(info.encrypt_key_size, u32::from_ne_bytes(*b"KEY!"));
        assert_eq!(info.flags, u32::from_ne_bytes(*b"FLAG"));
        assert_eq!(
            info.file_name,
            *b"NAME0001NAME0002NAME0003NAME0004NAME0005NAME0006NAME0007NAME0008"
        );
        assert_eq!(
            info.crypt_name,
            *b"CRYPT001CRYPT002CRYPT003CRYPT004CRYPT005CRYPT006CRYPT007CRYPT008"
        );
        assert_eq!(info.encrypt_key, *b"KEY_0001KEY_0002KEY_0003KEY_0004");
        assert_eq!(
            info.init,
            [
                u64::from_ne_bytes(*b"INIT0001"),
                u64::from_ne_bytes(*b"INIT0002")
            ]
        );
        assert_eq!(core::mem::size_of::<LoopInfo64>(), 232);
        assert_eq!(
            [
                core::mem::offset_of!(LoopInfo64, device),
                core::mem::offset_of!(LoopInfo64, inode),
                core::mem::offset_of!(LoopInfo64, rdevice),
                core::mem::offset_of!(LoopInfo64, offset),
                core::mem::offset_of!(LoopInfo64, sizelimit),
                core::mem::offset_of!(LoopInfo64, number),
                core::mem::offset_of!(LoopInfo64, encrypt_type),
                core::mem::offset_of!(LoopInfo64, encrypt_key_size),
                core::mem::offset_of!(LoopInfo64, flags),
                core::mem::offset_of!(LoopInfo64, file_name),
                core::mem::offset_of!(LoopInfo64, crypt_name),
                core::mem::offset_of!(LoopInfo64, encrypt_key),
                core::mem::offset_of!(LoopInfo64, init),
            ],
            [0, 8, 16, 24, 32, 40, 44, 48, 52, 56, 120, 184, 216]
        );
    }

    /// 测试目标：验证查询结构的身份、标志和文件名字节，以及其他位置的清零。
    /// 场景步骤：使用非零文件属性和完整文件名，分别编码两种支持标志及其组合。
    /// 预期结果：232 个输出字节与独立预期完全一致，窗口和加密字段不泄漏其他状态。
    #[test]
    fn encode_query_without_leaks() {
        let time = InodeTime {
            seconds: 123,
            nanoseconds: 456,
        };
        let stat = FileStat {
            inode: u64::from_ne_bytes(*b"ino-5678"),
            kind: InodeKind::Regular,
            mode: 0o100644,
            size: 8192,
            uid: 17,
            gid: 29,
            links: 2,
            blocks: 16,
            device: u64::from_ne_bytes(*b"dev-1234"),
            rdev: u64::from_ne_bytes(*b"rdv-9012"),
            block_size: 4096,
            atime: time,
            mtime: time,
            ctime: time,
        };
        let file_name = *b"FILE0001FILE0002FILE0003FILE0004FILE0005FILE0006FILE0007FILE0008";
        let mut expected = [0u8; 232];
        expected[0..8].copy_from_slice(b"dev-1234");
        expected[8..16].copy_from_slice(b"ino-5678");
        expected[16..24].copy_from_slice(b"rdv-9012");
        expected[56..120]
            .copy_from_slice(b"FILE0001FILE0002FILE0003FILE0004FILE0005FILE0006FILE0007FILE0008");

        assert_eq!(LO_FLAGS_READ_ONLY, 1);
        assert_eq!(LO_FLAGS_AUTOCLEAR, 4);
        for (flags, raw_flags) in [
            (0, 0u32),
            (LO_FLAGS_READ_ONLY, 1),
            (LO_FLAGS_AUTOCLEAR, 4),
            (LO_FLAGS_READ_ONLY | LO_FLAGS_AUTOCLEAR, 5),
        ] {
            expected[52..56].copy_from_slice(&raw_flags.to_ne_bytes());
            assert_eq!(
                LoopInfo64::status(stat, flags, file_name).to_bytes(),
                expected
            );
        }
    }

    /// 测试目标：验证允许的标志和当前不支持的窗口、标志及加密字段。
    /// 场景步骤：从有效的原始结构逐一设置标志和各字段的首尾字节。
    /// 预期结果：READ_ONLY、AUTOCLEAR 及其组合被接受，非零窗口、未知标志和任一加密字段均返回 `Unsupported`。
    #[test]
    fn supported_status_fields() {
        let mut bytes = [0u8; 232];
        bytes[0..24].copy_from_slice(b"DEV12345INO12345RDEV1234");
        bytes[40..44].copy_from_slice(b"NUM!");
        bytes[56..64].copy_from_slice(b"disk.img");

        for flags in [0u32, 1, 4, 5] {
            bytes[52..56].copy_from_slice(&flags.to_ne_bytes());
            assert_eq!(LoopInfo64::from_bytes(&bytes).check_supported(), Ok(()));
        }
        for unsupported in [2u32, 8, 16, 0x20, 0x8000_0000] {
            for flags in [unsupported, 5 | unsupported] {
                bytes[52..56].copy_from_slice(&flags.to_ne_bytes());
                assert_eq!(
                    LoopInfo64::from_bytes(&bytes).check_supported(),
                    Err(FsError::Unsupported),
                    "unsupported flags {:#x}",
                    flags
                );
            }
        }

        bytes[52..56].copy_from_slice(&5u32.to_ne_bytes());
        for index in [
            24usize, 31, 32, 39, 44, 47, 48, 51, 120, 183, 184, 215, 216, 223, 224, 231,
        ] {
            let mut invalid = bytes;
            invalid[index] = 1;
            assert_eq!(
                LoopInfo64::from_bytes(&invalid).check_supported(),
                Err(FsError::Unsupported),
                "nonzero unsupported field byte {}",
                index
            );
        }
    }
}
