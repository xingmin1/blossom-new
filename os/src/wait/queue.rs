//! 源队列只操作固定节点；移出 Waker 并解锁后才进入调度器。

use core::cell::UnsafeCell;
use core::marker::PhantomPinned;
use core::pin::Pin;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::sync::SpinLock;
use crate::wait::Waker;

struct NodeData {
    previous: *const WaitNode,
    next: *const WaitNode,
    waker: Option<Waker>,
}

/// 链接与通知值只经所属源锁访问；Pin 保持栈上节点地址，原子成员供析构核对。
pub struct WaitNode {
    data: UnsafeCell<NodeData>,
    linked: AtomicBool,
    _pinned: PhantomPinned,
}

impl WaitNode {
    /// 准备未登记节点，不分配存储；每个节点只登记一次。
    pub fn new(waker: Waker) -> Self {
        Self {
            data: UnsafeCell::new(NodeData {
                previous: ptr::null(),
                next: ptr::null(),
                waker: Some(waker),
            }),
            linked: AtomicBool::new(false),
            _pinned: PhantomPinned,
        }
    }
}

impl Drop for WaitNode {
    /// 检查析构前已撤链的不变量，不承担取消责任，也不替代 register 的存储存活前提。
    fn drop(&mut self) {
        assert!(
            !self.linked.load(Ordering::Acquire),
            "dropping a registered wait node"
        );
    }
}

struct Queue {
    head: *const WaitNode,
    tail: *const WaitNode,
    members: usize,
}

// SAFETY: 源锁串行全部节点访问；unsafe register 的调用者保证节点存储和源存活直到真实撤链，
// registration 的共享 Pin 借用保护正常路径，所有修改限制在 UnsafeCell 与原子成员内。
unsafe impl Send for Queue {}

impl Queue {
    /// 零节点队列无动态容器，头尾为空时允许正常移动。
    const fn new() -> Self {
        Self {
            head: ptr::null(),
            tail: ptr::null(),
            members: 0,
        }
    }

    /// 在源锁内尾插由 registration 借用保护的固定节点。
    fn push(&mut self, node: Pin<&WaitNode>) {
        let node = node.get_ref();
        assert!(
            !node.linked.load(Ordering::Relaxed),
            "wait node already registered"
        );
        let members = self
            .members
            .checked_add(1)
            .expect("wait queue member count overflow");
        // SAFETY: 独占源锁；Pin 借用保护新节点且它尚未在其他源登记，已有 tail 受各 registration 保活。
        unsafe {
            let data = &mut *node.data.get();
            assert!(data.waker.is_some(), "wait node already consumed");
            data.previous = self.tail;
            data.next = ptr::null();
            if self.tail.is_null() {
                self.head = node;
            } else {
                (*(*self.tail).data.get()).next = node;
            }
            self.tail = node;
        }
        self.members = members;
        node.linked.store(true, Ordering::Relaxed);
    }

    /// 锁内摘离并移出值；Release 清成员位是通知方对该节点的最后一次访问。
    fn remove(&mut self, node: &WaitNode) -> Option<Waker> {
        if !node.linked.load(Ordering::Relaxed) {
            return None;
        }
        let members = self
            .members
            .checked_sub(1)
            .expect("wait queue member count underflow");
        // SAFETY: 私有调用点只传本源成员，源锁串行访问链接；Pin registration 保护成员与邻居存储。
        let waker = unsafe {
            let data = &mut *node.data.get();
            if data.previous.is_null() {
                self.head = data.next;
            } else {
                (*(*data.previous).data.get()).next = data.next;
            }
            if data.next.is_null() {
                self.tail = data.previous;
            } else {
                (*(*data.next).data.get()).previous = data.previous;
            }
            data.previous = ptr::null();
            data.next = ptr::null();
            data.waker
                .take()
                .expect("registered wait node has no waker")
        };
        self.members = members;
        node.linked.store(false, Ordering::Release);
        Some(waker)
    }

    /// 只把移出的通知值交给锁外调用者，不留下解锁后访问栈节点的裸指针。
    fn pop(&mut self) -> Option<Waker> {
        if self.head.is_null() {
            return None;
        }
        // SAFETY: head 是仍登记的固定节点，源锁与 registration 保证它在本次摘离中有效。
        let node = unsafe { &*self.head };
        self.remove(node)
    }
}

/// registration 借用源，正常登记期间不能移动或释放含节点的队列。
pub struct WaitQueue {
    inner: SpinLock<Queue>,
}

impl WaitQueue {
    /// 准备可静态初始化的源队列，无等待控制对象分配。
    pub const fn new() -> Self {
        Self {
            inner: SpinLock::new(Queue::new()),
        }
    }

    /// 借用固定节点和源直到唯一 registration 析构；长期借用转为共享 Pin，内部变化只经 UnsafeCell。
    ///
    /// # Safety
    ///
    /// 调用者必须保持源与节点存储有效，直到该成员已在源锁内真实撤链。
    /// 活 registration 不得被遗忘；正常退出先析构 registration，再释放节点与源。
    /// Pin 借用无法阻止 mem::forget 提前结束借用，析构断言不能替代此前没有悬空或别名的保证。
    pub unsafe fn register<'a>(&'a self, node: Pin<&'a mut WaitNode>) -> QueueRegistration<'a> {
        let node = node.into_ref();
        self.inner.lock().push(node);
        QueueRegistration { queue: self, node }
    }

    /// 唤醒一个实际接受通知的成员；旧轮次或重复通知不消耗唤醒配额。
    pub fn wake_one(&self) -> bool {
        loop {
            let waker = self.inner.lock().pop();
            let Some(waker) = waker else {
                return false;
            };
            if waker.notify() {
                return true;
            }
        }
    }

    /// 最多摘离入口成员数量，通知与 Weak 析构在锁外；并发退列时不保证精确的入口身份集合。
    pub fn wake_all(&self) -> usize {
        self.wake_all_after_notify(|| {})
    }

    /// 测试在每次真实摘离并锁外通知后控制重新登记，不替代源队列或调度器。
    #[cfg(feature = "execution-tests")]
    pub fn test_wake_all_after_notify(&self, after: impl FnMut()) -> usize {
        self.wake_all_after_notify(after)
    }

    /// 普通唤醒与确定性交错共用入口成员数预算，每个实际 pop 消耗一次，拒绝通知不延长遍历。
    fn wake_all_after_notify(&self, mut after: impl FnMut()) -> usize {
        let mut remaining = self.inner.lock().members;
        let mut accepted = 0;
        while 0 < remaining {
            let waker = self.inner.lock().pop();
            let Some(waker) = waker else {
                return accepted;
            };
            remaining -= 1;
            if waker.notify() {
                accepted += 1;
            }
            drop(waker);
            after();
        }
        accepted
    }
}

impl Drop for WaitQueue {
    /// 检查源析构前已空的不变量，不扫描成员或执行第二次清理。
    fn drop(&mut self) {
        let queue = self.inner.lock();
        assert_eq!(
            queue.head.is_null(),
            queue.members == 0,
            "wait queue head and member count disagree"
        );
        assert_eq!(queue.members, 0, "dropping a nonempty wait queue");
    }
}

/// 持有节点的共享 Pin 与源借用，任何正常退出都先撤剩余链接再释放栈存储。
pub struct QueueRegistration<'a> {
    queue: &'a WaitQueue,
    node: Pin<&'a WaitNode>,
}

impl Drop for QueueRegistration<'_> {
    /// 唯一撤链责任；已被 wake 摘走时不再取值，剩余 Weak 在源锁外析构。
    fn drop(&mut self) {
        let waker = self.queue.inner.lock().remove(self.node.get_ref());
        drop(waker);
    }
}
