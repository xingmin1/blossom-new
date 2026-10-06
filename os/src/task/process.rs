//! 有限用户身份域与退出亲缘；图只弱关联执行者，僵尸记录不拥有运行资源。

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::pin::pin;
use core::ptr;

use crate::sched;
use crate::sync::SpinLock;
use crate::task::{JoinObserver, TaskId, UserThread};
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
    tid: Arc<Pid>,
}

impl UserIdentity {
    /// 每个成员保留自己的不可变TID，组首线程与TGID共享同一Pid对象。
    pub(crate) fn tid(&self) -> usize {
        self.tid.number
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
        self.tid.clone()
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

/// 每个真实执行者的离栈观察；节点锁外准备，退出后只保小完成对象。
pub(crate) struct PreparedExecution {
    join: JoinObserver,
    next: Option<Box<PreparedExecution>>,
}

impl PreparedExecution {
    /// 在身份发表前准备唯一成员节点，不将Task或用户运行资源带入僵尸记录。
    pub(crate) fn try_new(join: JoinObserver) -> Result<Box<Self>, ProcessError> {
        Box::try_new(Self { join, next: None }).map_err(|_| ProcessError::NoMemory)
    }
}

struct ThreadEntry {
    user: Weak<UserThread>,
    group: Arc<ThreadGroup>,
    execution: Box<PreparedExecution>,
}

struct ProcessEntry {
    group: Arc<ThreadGroup>,
    // 父组最后成员退出时转交根回收者，父数值复用前解除所有子引用。
    parent: Option<usize>,
    members: usize,
    leader_task_id: TaskId,
    leader_status: Option<i32>,
    exit_requested: Option<i32>,
    user_interrupts: usize,
    user_timer_interrupts: usize,
    completed: Option<Box<PreparedExecution>>,
    exit: Option<WaitEvent>,
}

impl ProcessEntry {
    /// 内核协调者消费进程事件后等待各成员真正离栈，不把用户退出事件当栈已归还。
    fn join_members(&mut self) {
        while let Some(mut execution) = self.completed.take() {
            self.completed = execution.next.take();
            execution.join.join();
        }
    }
}

impl Drop for ProcessEntry {
    /// 仅迭代释放小完成观察，不递归展开成员数，也不在用户wait中等待离栈。
    fn drop(&mut self) {
        while let Some(mut execution) = self.completed.take() {
            self.completed = execution.next.take();
        }
    }
}

struct CollectedChild {
    event: WaitEvent,
    process: Option<ProcessEntry>,
    _released: [Option<Arc<Pid>>; 3],
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

/// 固定号码索引保护四角色与亲缘，成员节点在发表前准备，退出只保未确认离栈的观察。
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

    /// 新进程私有预留号码，锁外完成身份分配；None指定内核根回收者。
    pub(crate) fn prepare_process(
        self: &Arc<Self>,
        parent: Option<&Arc<ThreadGroup>>,
    ) -> Result<PreparedIdentity, ProcessError> {
        let reservation = self.reserve_number(parent)?;
        let pid = Arc::try_new(Pid {
            number: reservation.index + 1,
        })
        .map_err(|_| ProcessError::NoMemory)?;
        let (pgid, sid) = match parent {
            Some(parent) => (parent.pgid.clone(), parent.sid.clone()),
            None => (pid.clone(), pid.clone()),
        };
        let group = Arc::try_new(ThreadGroup {
            graph: Arc::downgrade(self),
            pid: pid.clone(),
            pgid,
            sid,
        })
        .map_err(|_| ProcessError::NoMemory)?;
        let identity = Arc::try_new(UserIdentity {
            graph: self.clone(),
            group,
            tid: pid,
        })
        .map_err(|_| ProcessError::NoMemory)?;
        Ok(PreparedIdentity {
            identity,
            parent: parent.cloned(),
            reservation,
        })
    }

    /// 新线程只预留自己的TID，保相同组身份，最终发表再次确认组仍有活成员。
    pub(crate) fn prepare_thread(
        self: &Arc<Self>,
        group: &Arc<ThreadGroup>,
    ) -> Result<PreparedIdentity, ProcessError> {
        let reservation = self.reserve_number(Some(group))?;
        let tid = Arc::try_new(Pid {
            number: reservation.index + 1,
        })
        .map_err(|_| ProcessError::NoMemory)?;
        let identity = Arc::try_new(UserIdentity {
            graph: self.clone(),
            group: group.clone(),
            tid,
        })
        .map_err(|_| ProcessError::NoMemory)?;
        Ok(PreparedIdentity {
            identity,
            parent: None,
            reservation,
        })
    }

    /// 同一关系锁中核对真实组并取得唯一空号，后续对象准备不持图锁。
    fn reserve_number(
        self: &Arc<Self>,
        parent: Option<&Arc<ThreadGroup>>,
    ) -> Result<NumberReservation, ProcessError> {
        self.prune_completed();
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
        Ok(NumberReservation {
            graph: self.clone(),
            index,
            active: true,
        })
    }

    /// 再次创建前摘走已确认离栈的观察，不让活组积累历史完成对象。
    fn prune_completed(&self) {
        let mut discarded = None;
        {
            let mut slots = self.slots.lock();
            for process in slots.iter_mut().filter_map(|slot| slot.process.as_mut()) {
                let mut link = &mut process.completed;
                while let Some(execution) = link.as_ref() {
                    if execution.join.try_join().is_some() {
                        let mut execution = link.take().expect("completed member disappeared");
                        *link = execution.next.take();
                        execution.next = discarded.take();
                        discarded = Some(execution);
                    } else {
                        link = &mut link.as_mut().expect("member disappeared").next;
                    }
                }
            }
        }
        while let Some(mut execution) = discarded.take() {
            discarded = execution.next.take();
        }
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

    /// 仅请求成员在自己的合法用户返回点退出，首个组退出原因保留到最后成员结束。
    pub(crate) fn request_exit(&self, identity: &UserIdentity, status: i32) {
        let mut slots = self.slots.lock();
        let process = slots[identity.tgid() - 1]
            .process
            .as_mut()
            .expect("group exit requested for an unpublished process");
        assert!(Arc::ptr_eq(&process.group, &identity.group));
        assert!(
            0 < process.members,
            "group exit requested after last member"
        );
        process.exit_requested.get_or_insert(status);
    }

    /// 当前执行者在IRQ/业务短锁外检查本组控制，不让远核修改用户现场或释放栈。
    pub(crate) fn exit_request(&self, identity: &UserIdentity) -> Option<i32> {
        let slots = self.slots.lock();
        let process = slots[identity.tgid() - 1]
            .process
            .as_ref()
            .expect("running user has no process");
        assert!(Arc::ptr_eq(&process.group, &identity.group));
        process.exit_requested
    }

    /// 解绑本成员运行资源后移出弱关联；最后成员才发表组事件并转亲，小完成观察保至回收。
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
        let (user, group, released, last) = {
            let mut slots = self.slots.lock();
            let ThreadEntry {
                user,
                group,
                mut execution,
            } = slots[identity.tid() - 1]
                .thread
                .take()
                .expect("exiting thread is not published");
            assert!(
                Arc::ptr_eq(&group, &identity.group),
                "exiting stale thread identity"
            );
            let process = slots[identity.tgid() - 1]
                .process
                .as_mut()
                .expect("exiting thread has no process");
            assert!(
                Arc::ptr_eq(&process.group, &identity.group),
                "exiting stale group"
            );
            assert!(process.exit.is_none(), "exiting a reaped process");
            if identity.tid() == identity.tgid() {
                process.leader_status = Some(status);
            }
            process.members = process
                .members
                .checked_sub(1)
                .expect("group member underflow");
            process.user_interrupts = process
                .user_interrupts
                .checked_add(user_interrupts)
                .expect("group user interrupt count overflow");
            process.user_timer_interrupts = process
                .user_timer_interrupts
                .checked_add(user_timer_interrupts)
                .expect("group user timer count overflow");
            execution.next = process.completed.take();
            process.completed = Some(execution);
            let last = process.members == 0;
            if last {
                process.exit = Some(WaitEvent {
                    pid: identity.tgid(),
                    status: process
                        .exit_requested
                        .or(process.leader_status)
                        .unwrap_or(status),
                    user_interrupts: process.user_interrupts,
                    user_timer_interrupts: process.user_timer_interrupts,
                    task_id: process.leader_task_id,
                    cpu: crate::cpu::current().id,
                });
            }
            let released = detach_role(&mut slots, &identity.tid, TID);
            if last {
                for slot in slots.iter_mut() {
                    if let Some(child) = slot.process.as_mut() {
                        if child.parent == Some(identity.tgid()) {
                            child.parent = None;
                        }
                    }
                }
            }
            (user, group, released, last)
        };
        drop(user);
        drop(group);
        drop(released);
        if last {
            self.waiters.wake_all();
        }
    }

    /// 用户等待只消费退出信息，不等待成员离栈；nowait保留事件，具体子PID只允许一次消费。
    pub(crate) fn try_wait(
        &self,
        parent: Option<&ThreadGroup>,
        pid: usize,
        nowait: bool,
    ) -> Result<Option<WaitEvent>, ProcessError> {
        self.collect_child(parent, pid, nowait)
            .map(|child| child.map(|child| child.event))
    }

    /// 同锁核对亲缘并移出完整回收快照，通知与所有最后拥有均在关系锁外。
    fn collect_child(
        &self,
        parent: Option<&ThreadGroup>,
        pid: usize,
        nowait: bool,
    ) -> Result<Option<CollectedChild>, ProcessError> {
        let collected = {
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
                return Ok(Some(CollectedChild {
                    event,
                    process: None,
                    _released: [None, None, None],
                }));
            }
            assert_eq!(process.members, 0, "reaping a live group");
            assert!(slot.thread.is_none(), "reaping a live leader");
            let retired = slots[index]
                .process
                .take()
                .expect("wait process disappeared");
            let released = [
                detach_role(&mut slots, &retired.group.pid, TGID),
                detach_role(&mut slots, &retired.group.pgid, PGID),
                detach_role(&mut slots, &retired.group.sid, SID),
            ];
            CollectedChild {
                event,
                process: Some(retired),
                _released: released,
            }
        };
        self.waiters.wake_all();
        Ok(Some(collected))
    }

    /// 内核协调者先消费组事件，再在原登记已撤除后确认各成员离栈，不能嵌套两个Waiter。
    pub(crate) fn wait(
        &self,
        parent: Option<&ThreadGroup>,
        pid: usize,
    ) -> Result<WaitEvent, ProcessError> {
        let mut collected = self.wait_collection(parent, pid)?;
        collected
            .process
            .as_mut()
            .expect("wait collected no process")
            .join_members();
        Ok(collected.event)
    }

    /// 登记后复查关闭早通知窗口；返回拥有快照时本栈登记与Waiter已全部结束。
    fn wait_collection(
        &self,
        parent: Option<&ThreadGroup>,
        pid: usize,
    ) -> Result<CollectedChild, ProcessError> {
        sched::assert_switchable();
        loop {
            if let Some(collected) = self.collect_child(parent, pid, false)? {
                return Ok(collected);
            }
            let waiter = Waiter::new();
            let mut node = pin!(WaitNode::new(waiter.waker()));
            // SAFETY: self借用保活固定源；node固定当前Task栈，registration先于
            // node/waiter析构，复查关闭早通知窗口，返回前撤链后才可等待成员join。
            let _registration = unsafe { self.waiters.register(node.as_mut()) };
            if let Some(collected) = self.collect_child(parent, pid, false)? {
                return Ok(collected);
            }
            waiter.park();
        }
    }

    /// 内核根协调者逐个回收转亲后的真实进程树，每次仍等待原成员离栈。
    pub(crate) fn wait_root(&self) -> Result<WaitEvent, ProcessError> {
        let pid = {
            let slots = self.slots.lock();
            slots
                .iter()
                .filter_map(|slot| slot.process.as_ref())
                .find(|process| process.parent.is_none())
                .map(|process| process.group.tgid())
                .ok_or(ProcessError::NoChild)?
        };
        self.wait(None, pid)
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
            || process.members == 0
            || process.exit_requested.is_some()
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

    /// 最终关系发表重查父或当前组；新成员只挂TID，进程级角色与亲缘仅由组首发表。
    pub(crate) fn publish(
        mut self,
        user: Weak<UserThread>,
        execution: Box<PreparedExecution>,
    ) -> Result<(), ProcessError> {
        let graph = self.reservation.graph.clone();
        {
            let mut slots = graph.slots.lock();
            let leader = self.identity.tid() == self.identity.tgid();
            let parent = if leader {
                match self.parent.as_ref() {
                    Some(parent) => Some(graph.live_parent(&slots, parent)?),
                    None => None,
                }
            } else {
                graph.live_parent(&slots, &self.identity.group)?;
                None
            };
            let index = self.reservation.index;
            assert!(slots[index].reserved, "PID reservation disappeared");
            assert!(slots[index].pid.is_none(), "reserved PID already published");
            assert!(
                execution.next.is_none(),
                "prepared member is already linked"
            );
            let task_id = execution.join.task_id();
            slots[index].reserved = false;
            slots[index].pid = Some(self.identity.tid.clone());
            attach_role(&mut slots, &self.identity.tid, TID);
            slots[index].thread = Some(ThreadEntry {
                user,
                group: self.identity.group.clone(),
                execution,
            });
            if leader {
                attach_role(&mut slots, &self.identity.group.pid, TGID);
                attach_role(&mut slots, &self.identity.group.pgid, PGID);
                attach_role(&mut slots, &self.identity.group.sid, SID);
                slots[index].process = Some(ProcessEntry {
                    group: self.identity.group.clone(),
                    parent,
                    members: 1,
                    leader_task_id: task_id,
                    leader_status: None,
                    exit_requested: None,
                    user_interrupts: 0,
                    user_timer_interrupts: 0,
                    completed: None,
                    exit: None,
                });
            } else {
                let process = slots[self.identity.tgid() - 1]
                    .process
                    .as_mut()
                    .expect("thread published without a process");
                process.members = process
                    .members
                    .checked_add(1)
                    .expect("group member overflow");
            }
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
