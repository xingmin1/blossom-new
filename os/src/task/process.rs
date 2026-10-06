//! 有限用户身份域与退出亲缘；图只弱关联执行者，僵尸记录不拥有运行资源。

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::pin::pin;
use core::ptr;

use crate::sched;
use crate::sync::SpinLock;
use crate::task::{TaskId, UserThread};
use crate::wait::{WaitNode, WaitQueue, Waiter};

const TID: usize = 0;
const TGID: usize = 1;
const PGID: usize = 2;
const SID: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProcessError {
    InvalidCapacity,
    NoMemory,
    NoPid,
    InvalidParent,
    NoChild,
}

/// 数字不可变的身份对象；诊断引用不参与号码占用判定。
pub(crate) struct Pid {
    number: usize,
}

impl Pid {
    /// 读取该对象原来的数值，即使它的最后一个角色已解绑也不改变。
    pub(crate) fn number(&self) -> usize {
        self.number
    }
}

/// 组的不可变身份；成员、亲缘和退出状态由所属图的同一关系锁保护。
pub(crate) struct ThreadGroup {
    graph: Weak<TaskGraph>,
    pid: Arc<Pid>,
    pgid: Arc<Pid>,
    sid: Arc<Pid>,
}

impl ThreadGroup {
    /// 返回线程组身份，退出后诊断对象仍保留原值。
    pub(crate) fn tgid(&self) -> usize {
        self.pid.number
    }

    /// 返回根组自身建立或从父组继承的进程组身份。
    pub(crate) fn pgid(&self) -> usize {
        self.pgid.number
    }

    /// 返回根组自身建立或从父组继承的会话身份。
    pub(crate) fn sid(&self) -> usize {
        self.sid.number
    }
}

/// 执行者拥有自己的图与组；图反向只持 Weak<UserThread>，没有强引用环。
pub(crate) struct UserIdentity {
    graph: Arc<TaskGraph>,
    group: Arc<ThreadGroup>,
}

impl UserIdentity {
    /// 当前实际消费者创建组首线程，其 TID 与 TGID 指向同一身份对象。
    pub(crate) fn tid(&self) -> usize {
        self.group.pid.number
    }

    /// 返回用户线程所属线程组的 Linux 数字身份。
    pub(crate) fn tgid(&self) -> usize {
        self.group.tgid()
    }

    /// 取得不持 MM、文件或执行栈的线程组引用。
    pub(crate) fn group(&self) -> Arc<ThreadGroup> {
        self.group.clone()
    }

    /// 借用负责当前用户身份关系的原图。
    pub(crate) fn graph(&self) -> &Arc<TaskGraph> {
        &self.graph
    }

    /// 保存稳定身份对象供诊断；数值复用后此对象不会命中新执行者。
    pub(crate) fn tid_object(&self) -> Arc<Pid> {
        self.group.pid.clone()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WaitEvent {
    pub pid: usize,
    pub status: i32,
    pub user_interrupts: usize,
    pub user_timer_interrupts: usize,
    pub task_id: TaskId,
    pub cpu: usize,
}

struct ThreadEntry {
    user: Weak<UserThread>,
    task_id: TaskId,
}

struct ProcessEntry {
    group: Arc<ThreadGroup>,
    // 父退出时同锁转交根回收者，故这个数值在复用前已不再被任何子引用。
    parent: Option<usize>,
    exit: Option<WaitEvent>,
}

struct Slot {
    reserved: bool,
    pid: Option<Arc<Pid>>,
    roles: [usize; 4],
    thread: Option<ThreadEntry>,
    process: Option<ProcessEntry>,
}

impl Slot {
    /// 所有槽在构造时锁外初始化，随后关系锁中不扩容。
    fn vacant() -> Self {
        Self {
            reserved: false,
            pid: None,
            roles: [0; 4],
            thread: None,
            process: None,
        }
    }
}

/// 一个短锁保护四角色、真实执行成员与父子关系；所有存储都有构造时的上限。
pub(crate) struct TaskGraph {
    // ponytail: 有限号码域使用线性预留与转亲扫描；大号码域再换独立空闲索引。
    slots: SpinLock<Vec<Slot>>,
    waiters: WaitQueue,
}

/// 未发表号码的唯一归还责任，任何锁外准备失败都正常清除预留。
struct NumberReservation {
    graph: Arc<TaskGraph>,
    index: usize,
    active: bool,
}

/// 身份与运行资源分别准备；只有成功发表后才允许原 PreparedTask 进入 Ready。
pub(crate) struct PreparedIdentity {
    identity: Arc<UserIdentity>,
    parent: Option<Arc<ThreadGroup>>,
    reservation: NumberReservation,
}

impl TaskGraph {
    /// 锁外预留整个有限号码域，1..=capacity 兼容有符号 Linux pid_t。
    pub(crate) fn try_new(capacity: usize) -> Result<Arc<Self>, ProcessError> {
        if capacity == 0 || (i32::MAX as usize) < capacity {
            return Err(ProcessError::InvalidCapacity);
        }
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(capacity)
            .map_err(|_| ProcessError::NoMemory)?;
        slots.resize_with(capacity, Slot::vacant);
        Arc::try_new(Self {
            slots: SpinLock::new(slots),
            waiters: WaitQueue::new(),
        })
        .map_err(|_| ProcessError::NoMemory)
    }

    /// 先私有预留号码，再锁外完成所有 fallible 身份分配；None 指定内核根回收者。
    pub(crate) fn prepare_process(
        self: &Arc<Self>,
        parent: Option<&Arc<ThreadGroup>>,
    ) -> Result<PreparedIdentity, ProcessError> {
        let index = {
            let mut slots = self.slots.lock();
            if let Some(parent) = parent {
                self.live_parent(&slots, parent)?;
            }
            let index = slots
                .iter()
                .position(|slot| !slot.reserved && slot.pid.is_none())
                .ok_or(ProcessError::NoPid)?;
            slots[index].reserved = true;
            index
        };
        let reservation = NumberReservation {
            graph: self.clone(),
            index,
            active: true,
        };
        let pid = Arc::try_new(Pid { number: index + 1 }).map_err(|_| ProcessError::NoMemory)?;
        let (pgid, sid) = match parent {
            Some(parent) => (parent.pgid.clone(), parent.sid.clone()),
            None => (pid.clone(), pid.clone()),
        };
        let group = Arc::try_new(ThreadGroup {
            graph: Arc::downgrade(self),
            pid,
            pgid,
            sid,
        })
        .map_err(|_| ProcessError::NoMemory)?;
        let identity = Arc::try_new(UserIdentity {
            graph: self.clone(),
            group,
        })
        .map_err(|_| ProcessError::NoMemory)?;
        Ok(PreparedIdentity {
            identity,
            parent: parent.cloned(),
            reservation,
        })
    }

    /// 锁内只快照已发表的弱执行关联；升级在锁外，不以 Ready 为查询条件。
    pub(crate) fn lookup_thread(&self, tid: usize) -> Option<Arc<UserThread>> {
        let user = {
            let slots = self.slots.lock();
            slots
                .get(tid.checked_sub(1)?)?
                .thread
                .as_ref()?
                .user
                .clone()
        };
        user.upgrade()
    }

    /// 同数值仍必须核对原 Pid 对象，旧诊断对象永远不会指向新同号线程。
    pub(crate) fn lookup_pid(&self, pid: &Arc<Pid>) -> Option<Arc<UserThread>> {
        let user = {
            let slots = self.slots.lock();
            let slot = slots.get(pid.number.checked_sub(1)?)?;
            if !slot
                .pid
                .as_ref()
                .is_some_and(|entry| Arc::ptr_eq(entry, pid))
            {
                return None;
            }
            slot.thread.as_ref()?.user.clone()
        };
        user.upgrade()
    }

    /// 调用者先解绑 MM/files/fs；发表正常退出或用户信号终止值，锁外释放执行关联并通知。
    pub(crate) fn exit(
        &self,
        identity: &UserIdentity,
        status: i32,
        user_interrupts: usize,
        user_timer_interrupts: usize,
    ) {
        assert!(
            ptr::eq(self, identity.graph.as_ref()),
            "identity belongs to another graph"
        );
        let signal = status & 0x7f;
        assert!(
            status & !0xffff == 0
                && (status & 0xff == 0 || (status & 0xff00 == 0 && (1..=64).contains(&signal))),
            "invalid Linux exit status"
        );
        let thread = {
            let mut slots = self.slots.lock();
            let index = identity.tid() - 1;
            let slot = &mut slots[index];
            let process = slot.process.as_mut().expect("exiting unpublished process");
            assert!(
                Arc::ptr_eq(&process.group, &identity.group),
                "exiting stale identity"
            );
            assert!(process.exit.is_none(), "process exited twice");
            let thread = slot
                .thread
                .take()
                .expect("exiting process has no live member");
            process.exit = Some(WaitEvent {
                pid: identity.tgid(),
                status,
                user_interrupts,
                user_timer_interrupts,
                task_id: thread.task_id,
                cpu: crate::cpu::current().id,
            });
            // 当前创建路径只有真实组首线程；移出它就是最后活成员退出。
            // 转亲在父数值可能复用前完成，子组继承的 PGID/SID 角色仍独立保留。
            for slot in slots.iter_mut() {
                if let Some(child) = slot.process.as_mut() {
                    if child.parent == Some(identity.tgid()) {
                        child.parent = None;
                    }
                }
            }
            thread
        };
        drop(thread);
        self.waiters.wake_all();
    }

    /// 查询具体非零子 PID；有子未退返回 None，无子返回 NoChild，nowait 不消费。
    pub(crate) fn try_wait(
        &self,
        parent: Option<&ThreadGroup>,
        pid: usize,
        nowait: bool,
    ) -> Result<Option<WaitEvent>, ProcessError> {
        let (event, retired, released) = {
            let mut slots = self.slots.lock();
            let parent_number = match parent {
                Some(parent) => Some(
                    self.live_parent(&slots, parent)
                        .map_err(|_| ProcessError::NoChild)?,
                ),
                None => None,
            };
            let index = pid.checked_sub(1).ok_or(ProcessError::NoChild)?;
            let slot = slots.get(index).ok_or(ProcessError::NoChild)?;
            let process = slot.process.as_ref().ok_or(ProcessError::NoChild)?;
            if process.parent != parent_number {
                return Err(ProcessError::NoChild);
            }
            let Some(event) = process.exit else {
                return Ok(None);
            };
            if nowait {
                return Ok(Some(event));
            }
            assert!(slot.thread.is_none(), "reaping a live group member");
            let retired = slots[index]
                .process
                .take()
                .expect("wait process disappeared");
            let group = &retired.group;
            let released = [
                detach_role(&mut slots, &group.pid, TID),
                detach_role(&mut slots, &group.pid, TGID),
                detach_role(&mut slots, &group.pgid, PGID),
                detach_role(&mut slots, &group.sid, SID),
            ];
            (event, retired, released)
        };
        drop(retired);
        drop(released);
        // 其他具体 PID 等待者应立即观察唯一消费之后的 NoChild。
        self.waiters.wake_all();
        Ok(Some(event))
    }

    /// 原 Waiter 先登记再复查，早通知与并发消费均不会漏醒；只在普通可切换 Task 调用。
    pub(crate) fn wait(
        &self,
        parent: Option<&ThreadGroup>,
        pid: usize,
    ) -> Result<WaitEvent, ProcessError> {
        sched::assert_switchable();
        loop {
            if let Some(event) = self.try_wait(parent, pid, false)? {
                return Ok(event);
            }
            let waiter = Waiter::new();
            let mut node = pin!(WaitNode::new(waiter.waker()));
            // SAFETY: self 的共享借用保活原等待源；node 固定于当前真实 Task 栈，
            // registration 不被遗忘且先于 node/waiter 析构撤链，登记后的复查关闭早通知窗口。
            let _registration = unsafe { self.waiters.register(node.as_mut()) };
            if let Some(event) = self.try_wait(parent, pid, false)? {
                return Ok(event);
            }
            waiter.park();
        }
    }

    /// 同关系锁核对原组身份和实际活成员；旧组对象与同号新组不能互为父子。
    fn live_parent(&self, slots: &[Slot], parent: &ThreadGroup) -> Result<usize, ProcessError> {
        if !ptr::eq(self, parent.graph.as_ptr()) {
            return Err(ProcessError::InvalidParent);
        }
        let slot = slots
            .get(parent.tgid() - 1)
            .ok_or(ProcessError::InvalidParent)?;
        let process = slot.process.as_ref().ok_or(ProcessError::InvalidParent)?;
        if !ptr::eq(process.group.as_ref(), parent)
            || process.exit.is_some()
            || slot.thread.is_none()
        {
            return Err(ProcessError::InvalidParent);
        }
        Ok(parent.tgid())
    }
}

impl PreparedIdentity {
    /// 供父模块在身份发表前绑定完整 UserThread 与原 PreparedTask 资源。
    pub(crate) fn identity(&self) -> Arc<UserIdentity> {
        self.identity.clone()
    }

    /// 短锁中重查父、发表四角色与弱执行索引；成功后父才可发表原任务的 Ready。
    pub(crate) fn publish(
        mut self,
        user: Weak<UserThread>,
        task_id: TaskId,
    ) -> Result<(), ProcessError> {
        let graph = self.reservation.graph.clone();
        {
            let mut slots = graph.slots.lock();
            let parent = match self.parent.as_ref() {
                Some(parent) => Some(graph.live_parent(&slots, parent)?),
                None => None,
            };
            let index = self.reservation.index;
            assert!(slots[index].reserved, "PID reservation disappeared");
            assert!(slots[index].pid.is_none(), "reserved PID already published");
            slots[index].reserved = false;
            slots[index].pid = Some(self.identity.group.pid.clone());
            attach_role(&mut slots, &self.identity.group.pid, TID);
            attach_role(&mut slots, &self.identity.group.pid, TGID);
            attach_role(&mut slots, &self.identity.group.pgid, PGID);
            attach_role(&mut slots, &self.identity.group.sid, SID);
            slots[index].thread = Some(ThreadEntry { user, task_id });
            slots[index].process = Some(ProcessEntry {
                group: self.identity.group.clone(),
                parent,
                exit: None,
            });
            self.reservation.active = false;
        }
        Ok(())
    }
}

impl Drop for NumberReservation {
    /// 未发表准备失败只归还自己的私有号码，不解绑任何已发表身份。
    fn drop(&mut self) {
        if self.active {
            let mut slots = self.graph.slots.lock();
            let slot = &mut slots[self.index];
            assert!(
                slot.reserved && slot.pid.is_none(),
                "invalid private PID reservation"
            );
            slot.reserved = false;
        }
    }
}

/// 已有身份槽中增加实际角色成员；不能通过 Arc 引用数推导号码是否占用。
fn attach_role(slots: &mut [Slot], pid: &Arc<Pid>, role: usize) {
    let slot = &mut slots[pid.number - 1];
    assert!(
        slot.pid
            .as_ref()
            .is_some_and(|entry| Arc::ptr_eq(entry, pid)),
        "attaching a stale PID role"
    );
    slot.roles[role] = slot.roles[role]
        .checked_add(1)
        .expect("PID role count overflow");
}

/// 最后实际角色解绑时立即释放数值；移出的身份引用由调用者在关系锁外析构。
fn detach_role(slots: &mut [Slot], pid: &Arc<Pid>, role: usize) -> Option<Arc<Pid>> {
    let slot = &mut slots[pid.number - 1];
    assert!(
        slot.pid
            .as_ref()
            .is_some_and(|entry| Arc::ptr_eq(entry, pid)),
        "detaching a stale PID role"
    );
    slot.roles[role] = slot.roles[role]
        .checked_sub(1)
        .expect("PID role count underflow");
    if slot.roles == [0; 4] {
        assert!(
            !slot.reserved && slot.thread.is_none() && slot.process.is_none(),
            "freeing a PID with attached identities"
        );
        slot.pid.take()
    } else {
        None
    }
}
