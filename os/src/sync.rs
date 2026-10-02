//! 短临界区只借用已有 spin 锁；锁释放以后才恢复当前 CPU 的原 IRQ 状态。

use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};

/// 保存当前 CPU 中断状态的局部临界区，不能跨 CPU 移动。
pub struct IrqSaveGuard {
    saved: usize,
    _local: PhantomData<*mut ()>,
}

impl IrqSaveGuard {
    /// 关闭本地 IRQ，并保存进入前的状态。
    pub fn new() -> Self {
        Self {
            saved: crate::arch::irq_save(),
            _local: PhantomData,
        }
    }
}

impl Drop for IrqSaveGuard {
    fn drop(&mut self) {
        unsafe { crate::arch::irq_restore(self.saved) };
    }
}

/// 保护不能睡眠或长期等待的共享状态。
pub struct SpinLock<T> {
    inner: spin::Mutex<T>,
}

impl<T> SpinLock<T> {
    /// 在静态初始化中建立一把具体对象的短锁。
    pub const fn new(value: T) -> Self {
        Self {
            inner: spin::Mutex::new(value),
        }
    }

    /// 先保存并关闭 IRQ，再进入锁域。
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let irq = IrqSaveGuard::new();
        let guard = ManuallyDrop::new(self.inner.lock());
        SpinLockGuard { guard, _irq: irq }
    }
}

/// 持锁期间保持进入前的 IRQ 状态，退出顺序为解锁、恢复 IRQ。
pub struct SpinLockGuard<'a, T> {
    guard: ManuallyDrop<spin::MutexGuard<'a, T>>,
    _irq: IrqSaveGuard,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        unsafe { ManuallyDrop::drop(&mut self.guard) };
    }
}
