//! 内核唯一的 ChaCha20 随机字节生成器。
//!
//! BSP 从只读固件设备树的 `/chosen/rng-seed` 取得至少 32 字节后，生成器才进入就绪状态。
//! `getrandom`、`/dev/urandom` 和 ELF `AT_RANDOM` 共同消费同一个受 IRQ-save 自旋锁保护的状态。
//! 未就绪时只允许显式不安全接口输出；计数器只用于初始状态区分，不计作可信熵。
//! 锁内仅执行内核切片填充，调用者必须在解锁后复制用户内存、访问文件或等待。
//!
//! 当前只使用启动种子，不建立运行期熵池、周期重播种或每 CPU 副本；设备树保持只读。

use rand_chacha::rand_core::{RngCore, SeedableRng};
use rand_chacha::ChaCha20Rng;

use crate::arch;
use crate::sync::SpinLock;

/// ChaCha20 密钥需要的可信种子字节数。
const SEED_BYTES: usize = 32;

/// 同一把锁保护生成器及其可信种子就绪状态。
struct RandomState {
    /// 首次不安全输出或可信种子安装时建立的唯一生成器。
    generator: Option<ChaCha20Rng>,
    /// 仅在安装足量固件种子后置位，计数器初始化不改变此字段。
    ready: bool,
}

static RANDOM: SpinLock<RandomState> = SpinLock::new(RandomState {
    generator: None,
    ready: false,
});

/// 由 BSP 在固件发现完成后安装启动种子；不足 32 字节时保持未就绪。
///
/// 调用者负责保证种子来自可信固件随机源；本函数只读取种子，不修改或擦除设备树。
/// 生产启动只调用一次，用户写入和测试设备树解析不得调用此入口。
pub(crate) fn seed_from_bootloader(bytes: &[u8]) {
    let Some(bytes) = bytes.get(..SEED_BYTES) else {
        return;
    };
    let mut seed = [0; SEED_BYTES];
    seed.copy_from_slice(bytes);
    let mut state = RANDOM.lock();
    assert!(!state.ready, "boot random seed installed twice");
    state.generator = Some(ChaCha20Rng::from_seed(seed));
    state.ready = true;
}

/// 从共同生成器填充内核切片；未就绪时仍输出，供 `AT_RANDOM` 与显式不安全读取使用。
///
/// 未获固件种子时，计数器初始化仅提供可预测的非阻塞输出，不使安全接口就绪。
/// 持 IRQ-save 自旋锁期间不分配、不睡眠、不访问用户地址。
pub(crate) fn fill_insecure(bytes: &mut [u8]) {
    if bytes.is_empty() {
        return;
    }
    let mut state = RANDOM.lock();
    let generator = state
        .generator
        .get_or_insert_with(|| ChaCha20Rng::seed_from_u64(arch::read_counter()));
    generator.fill_bytes(bytes);
}

/// 只在可信种子就绪后填充内核切片；未就绪返回 `false` 并保持每个目标字节不变。
///
/// 空切片可查询就绪状态；锁只覆盖状态检查和内核切片填充，不跨用户态复制。
pub(crate) fn try_fill_secure(bytes: &mut [u8]) -> bool {
    let mut state = RANDOM.lock();
    if !state.ready {
        return false;
    }
    state
        .generator
        .as_mut()
        .expect("ready random generator absent")
        .fill_bytes(bytes);
    true
}
