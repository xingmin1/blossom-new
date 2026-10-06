//! 有限用户身份域与退出亲缘；图只弱关联执行者，僵尸记录不拥有运行资源。

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::pin::pin;
use core::ptr;

use crate::sched;
use crate::sync::SpinLock;
use crate::task::signal::{
    Sighand, SignalAction, SignalControl, SignalError, SignalGroup, SignalInfo, SignalInheritance,
    SignalPayload, ThreadSignals,
};
use crate::task::time::{GroupTime, USER_HZ};
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
    Busy,
    NoProcess,
    Permission,
    Access,
    InvalidArgument,
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

/// 组身份对象保持不变；可变角色和退出信号由关系锁事务更新。
pub(crate) struct ThreadGroup {
    graph: Weak<TaskGraph>,
    pid: Arc<Pid>,
    relations: SpinLock<GroupRelations>,
    uid: u32,
    signals: Arc<SignalGroup>,
    time: Arc<GroupTime>,
}

struct GroupRelations {
    pgid: Arc<Pid>,
    sid: Arc<Pid>,
    exit_signal: u8,
}

impl ThreadGroup {
    /// 返回线程组身份，退出后诊断对象仍保留原值。
    pub(crate) fn tgid(&self) -> usize {
        self.pid.number
    }

    /// 返回根组自身建立或从父组继承的进程组身份。
    pub(crate) fn pgid(&self) -> usize {
        self.relations.lock().pgid.number
    }

    /// 在关系锁内取得当前PGID原对象，等待不得先取数字再回图查同号身份。
    pub(crate) fn pgid_object(&self) -> Arc<Pid> {
        Arc::clone(&self.relations.lock().pgid)
    }

    /// 返回根组自身建立或从父组继承的会话身份。
    pub(crate) fn sid(&self) -> usize {
        self.relations.lock().sid.number
    }

    /// 借用组独有pending与控制拥有，不把共享动作轴等同于线程组。
    pub(crate) fn signals(&self) -> &Arc<SignalGroup> {
        &self.signals
    }

    /// 借用真实组CPU时间和itimer拥有，线程私有时间由执行者另持。
    pub(crate) fn time(&self) -> &Arc<GroupTime> {
        &self.time
    }
}

/// 执行者拥有自己的图与组；图反向只持 Weak<UserThread>，没有强引用环。
pub(crate) struct UserIdentity {
    graph: Arc<TaskGraph>,
    group: Arc<ThreadGroup>,
    tid: SpinLock<Arc<Pid>>,
    signals: Arc<ThreadSignals>,
}

impl UserIdentity {
    /// 返回当前TID；exec可在保留本身份对象时接管原组首的Pid。
    pub(crate) fn tid(&self) -> usize {
        self.tid.lock().number
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
        self.tid.lock().clone()
    }

    /// 身份准备时已构造private信号状态，退出解绑运行资源不会丢失早到信号。
    pub(crate) fn signals(&self) -> Arc<ThreadSignals> {
        self.signals.clone()
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

/// 有限waitid消费的真实子组状态，停止事件只在全部实际成员到安全点后产生。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChildEvent {
    pub(crate) pid: usize,
    pub(crate) uid: u32,
    pub(crate) code: i32,
    pub(crate) status: i32,
    pub(crate) user_ns: u64,
    pub(crate) kernel_ns: u64,
}

/// 已由ABI解码的子进程选择条件，扫描和消费共用同一关系锁。
pub(crate) enum WaitSelector {
    Pid(Arc<Pid>),
    Group(Arc<Pid>),
    Any,
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
    signals: Arc<ThreadSignals>,
}

struct ProcessEntry {
    group: Arc<ThreadGroup>,
    // 父组最后成员退出时转交根回收者，父数值复用前解除所有子引用。
    parent: Option<usize>,
    members: usize,
    leader_task_id: TaskId,
    leader_status: Option<i32>,
    exec_owner: Option<TaskId>,
    did_exec: bool,
    user_interrupts: usize,
    user_timer_interrupts: usize,
    completed: Option<Box<PreparedExecution>>,
    exit: Option<WaitEvent>,
    control_event: Option<ChildEvent>,
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
    control_waiters: WaitQueue,
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
            control_waiters: WaitQueue::new(),
        })
        .map_err(|_| ProcessError::NoMemory)
    }

    /// 新进程私有预留号码与已解码的退出信号，锁外完成分配；None指定内核根回收者。
    pub(crate) fn prepare_process(
        self: &Arc<Self>,
        parent: Option<&Arc<ThreadGroup>>,
        sighand: Arc<Sighand>,
        uid: u32,
        inherited: SignalInheritance,
        exit_signal: u8,
    ) -> Result<PreparedIdentity, ProcessError> {
        let reservation = self.reserve_number(parent, false)?;
        let pid = Arc::try_new(Pid {
            number: reservation.index + 1,
        })
        .map_err(|_| ProcessError::NoMemory)?;
        let (pgid, sid) = match parent {
            Some(parent) => {
                let relations = parent.relations.lock();
                (relations.pgid.clone(), relations.sid.clone())
            }
            None => (pid.clone(), pid.clone()),
        };
        let signals = SignalGroup::try_new(Arc::downgrade(self), pid.number, sighand)
            .map_err(|_| ProcessError::NoMemory)?;
        let private = ThreadSignals::try_new(signals.clone(), inherited)
            .map_err(|_| ProcessError::NoMemory)?;
        let time = GroupTime::try_new(signals.clone()).map_err(|_| ProcessError::NoMemory)?;
        let group = Arc::try_new(ThreadGroup {
            graph: Arc::downgrade(self),
            pid: pid.clone(),
            relations: SpinLock::new(GroupRelations {
                pgid,
                sid,
                exit_signal,
            }),
            uid,
            signals,
            time,
        })
        .map_err(|_| ProcessError::NoMemory)?;
        let identity = Arc::try_new(UserIdentity {
            graph: self.clone(),
            group,
            tid: SpinLock::new(pid),
            signals: private,
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
        inherited: SignalInheritance,
    ) -> Result<PreparedIdentity, ProcessError> {
        let reservation = self.reserve_number(Some(group), true)?;
        let tid = Arc::try_new(Pid {
            number: reservation.index + 1,
        })
        .map_err(|_| ProcessError::NoMemory)?;
        let signals = ThreadSignals::try_new(group.signals.clone(), inherited)
            .map_err(|_| ProcessError::NoMemory)?;
        let identity = Arc::try_new(UserIdentity {
            graph: self.clone(),
            group: group.clone(),
            tid: SpinLock::new(tid),
            signals,
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
        thread: bool,
    ) -> Result<NumberReservation, ProcessError> {
        self.prune_completed();
        let index = {
            let mut slots = self.slots.lock();
            if let Some(parent) = parent {
                self.live_parent(&slots, parent)?;
                if thread
                    && slots[parent.tgid() - 1]
                        .process
                        .as_ref()
                        .expect("live group disappeared")
                        .exec_owner
                        .is_some()
                {
                    return Err(ProcessError::Busy);
                }
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

    /// 保存此刻已发表的原身份，阻塞等待不能因数值复用改选另一个对象。
    pub(crate) fn pid_object(&self, pid: usize) -> Option<Arc<Pid>> {
        self.slots.lock().get(pid.checked_sub(1)?)?.pid.clone()
    }

    /// 首个组退出原因保留，锁外通知所有真实成员在自己的合法用户返回点退出。
    pub(crate) fn request_exit(&self, identity: &UserIdentity, status: i32) {
        identity.group.signals.request_exit(status);
    }

    /// 当前执行者在IRQ和业务短锁外读取本组控制，不允许远核改用户现场或释放栈。
    pub(crate) fn exit_request(&self, identity: &UserIdentity) -> Option<i32> {
        identity.group.signals.exit_request()
    }

    /// 当前实际Task独占exec，其他成员在自己的合法点退出并真正离栈后才交接TID。
    pub(crate) fn de_thread(&self, identity: &UserIdentity) -> Result<(), ProcessError> {
        sched::assert_switchable();
        let owner = crate::task::current_task_id().ok_or(ProcessError::NoProcess)?;
        let capacity = {
            let mut slots = self.slots.lock();
            let process = self.exec_process(&mut slots, identity, owner)?;
            if process.exec_owner.is_some() {
                return Err(ProcessError::Busy);
            }
            process.exec_owner = Some(owner);
            slots.len()
        };
        for index in 0..capacity {
            let member = {
                let slots = self.slots.lock();
                slots[index]
                    .thread
                    .as_ref()
                    .filter(|thread| {
                        Arc::ptr_eq(&thread.group, &identity.group)
                            && thread.execution.join.task_id() != owner
                    })
                    .map(|thread| (thread.signals.clone(), thread.user.clone()))
            };
            if let Some((signals, user)) = member {
                signals.request_exec_exit();
                if let Some(user) = user.upgrade() {
                    user.kick();
                }
            }
        }
        // 已停止成员使用图的控制等待源，private通知之外还须唤醒其原停驻登记。
        self.control_waiters.wake_all();
        loop {
            let completed = {
                let mut slots = self.slots.lock();
                let process = self.exec_process(&mut slots, identity, owner)?;
                if process.exec_owner != Some(owner) {
                    return Err(ProcessError::Busy);
                }
                (process.members == 1).then(|| process.completed.take())
            };
            if let Some(mut completed) = completed {
                // 成员已解绑资源；此处没有本栈Waiter，逐个确认真正归还执行栈。
                while let Some(mut execution) = completed.take() {
                    completed = execution.next.take();
                    execution.join.join();
                }
                break;
            }
            let waiter = Waiter::new();
            let mut node = pin!(WaitNode::new(waiter.waker()));
            // SAFETY: 图和栈节点保活，登记先于node/waiter析构；登记后重查防止早通知。
            let _registration = unsafe { self.control_waiters.register(node.as_mut()) };
            {
                let mut slots = self.slots.lock();
                let process = self.exec_process(&mut slots, identity, owner)?;
                if process.exec_owner != Some(owner) {
                    return Err(ProcessError::Busy);
                }
                if process.members == 1 {
                    continue;
                }
            }
            waiter.park();
        }
        let released = {
            let mut slots = self.slots.lock();
            let process = self.exec_process(&mut slots, identity, owner)?;
            if process.exec_owner != Some(owner) || process.members != 1 {
                return Err(ProcessError::Busy);
            }
            process.leader_task_id = owner;
            process.leader_status = None;
            process.control_event = None;
            identity.group.relations.lock().exit_signal = 17;
            let mut tid = identity.tid.lock();
            if Arc::ptr_eq(&tid, &identity.group.pid) {
                (None, None)
            } else {
                let old_index = tid.number - 1;
                let new_index = identity.tgid() - 1;
                if slots[new_index].thread.is_some() {
                    return Err(ProcessError::Busy);
                }
                slots[new_index].thread = slots[old_index].thread.take();
                attach_role(&mut slots, &identity.group.pid, TID);
                let old = core::mem::replace(&mut *tid, identity.group.pid.clone());
                let released = detach_role(&mut slots, &old, TID);
                (Some(old), released)
            }
        };
        drop(released);
        Ok(())
    }

    /// 新映像已在同一Task发表后释放exec控制，并记录父setpgid不可越过的exec边界。
    pub(crate) fn complete_exec(&self, identity: &UserIdentity) -> Result<(), ProcessError> {
        let owner = crate::task::current_task_id().ok_or(ProcessError::NoProcess)?;
        {
            let mut slots = self.slots.lock();
            let process = self.exec_process(&mut slots, identity, owner)?;
            if process.exec_owner != Some(owner)
                || process.members != 1
                || identity.tid() != identity.tgid()
            {
                return Err(ProcessError::Busy);
            }
            process.did_exec = true;
            process.exec_owner = None;
        }
        self.control_waiters.wake_all();
        Ok(())
    }

    /// 关系锁内核对原组、私有身份和当前实际Task，不把同号复用或组退出视为成功。
    fn exec_process<'a>(
        &self,
        slots: &'a mut [Slot],
        identity: &UserIdentity,
        owner: TaskId,
    ) -> Result<&'a mut ProcessEntry, ProcessError> {
        if !ptr::eq(self, identity.graph.as_ref()) {
            return Err(ProcessError::NoProcess);
        }
        let thread = slots
            .get(
                identity
                    .tid()
                    .checked_sub(1)
                    .ok_or(ProcessError::NoProcess)?,
            )
            .and_then(|slot| slot.thread.as_ref())
            .ok_or(ProcessError::NoProcess)?;
        if !Arc::ptr_eq(&thread.group, &identity.group)
            || !Arc::ptr_eq(&thread.signals, &identity.signals)
            || thread.execution.join.task_id() != owner
        {
            return Err(ProcessError::NoProcess);
        }
        let process = slots
            .get_mut(identity.tgid() - 1)
            .and_then(|slot| slot.process.as_mut())
            .filter(|process| Arc::ptr_eq(&process.group, &identity.group))
            .ok_or(ProcessError::NoProcess)?;
        if process.members == 0
            || process.exit.is_some()
            || identity.group.signals.exit_request().is_some()
            || identity.signals.exec_exit_requested()
        {
            return Err(ProcessError::Busy);
        }
        Ok(process)
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
        let (user, group, released, exit, event, parent) = {
            let mut slots = self.slots.lock();
            let ThreadEntry {
                user,
                group,
                mut execution,
                signals,
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
            let task_id = execution.join.task_id();
            if identity.tid() == identity.tgid()
                && !signals.exec_exit_requested()
                && !process.exec_owner.is_some_and(|owner| owner != task_id)
            {
                process.leader_status = Some(status);
            }
            if process.exec_owner == Some(task_id) {
                process.exec_owner = None;
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
            let stopped = group
                .signals
                .remove_member(&signals)
                .map(|signum| child_event(&group, 5, signum as i32));
            if let Some(event) = stopped {
                process.control_event = Some(event);
            }
            let last = process.members == 0;
            let exit = last.then(|| WaitEvent {
                pid: identity.tgid(),
                status: group
                    .signals
                    .exit_request()
                    .or(process.leader_status)
                    .unwrap_or(status),
                user_interrupts: process.user_interrupts,
                user_timer_interrupts: process.user_timer_interrupts,
                task_id: process.leader_task_id,
                cpu: crate::cpu::current().id,
            });
            let event = exit
                .map(|exit| exit_child_event(&group, exit.status))
                .or(stopped);
            let parent_number = process.parent;
            let parent = parent_number
                .and_then(|pid| slots[pid - 1].process.as_ref())
                .map(|parent| parent.group.clone());
            let released = detach_role(&mut slots, &identity.tid_object(), TID);
            if last {
                for slot in slots.iter_mut() {
                    if let Some(child) = slot.process.as_mut() {
                        if child.parent == Some(identity.tgid()) {
                            child.parent = None;
                        }
                    }
                }
            }
            (user, group, released, exit, event, parent)
        };
        drop(user);
        drop(released);
        if let Some(exit) = exit {
            // 先在锁外确认定时回调尾部，随后才允许用户wait回收组身份与时间拥有。
            group.time.stop();
            let mut slots = self.slots.lock();
            let process = slots[identity.tgid() - 1]
                .process
                .as_mut()
                .expect("last exiting group disappeared before timer stop");
            assert!(Arc::ptr_eq(&process.group, &group));
            assert_eq!(process.members, 0);
            process.exit = Some(exit);
        }
        if let Some(event) = event {
            send_parent_event(parent.as_deref(), &group, event);
            self.waiters.wake_all();
        }
        self.control_waiters.wake_all();
        drop(group);
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
            if let Some(parent) = parent {
                parent.time.add_reaped_child(&process.group.time);
            }
            reap_child(&mut slots, index)
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

    /// 用户waitid登记沿用原退出通知源，业务事件在登记后仍须复查。
    pub(crate) fn waiters(&self) -> &WaitQueue {
        &self.waiters
    }

    /// 内核根回收者关闭现存组的合法点执行，统一标记后锁外提醒，派生发表重新核对退出请求。
    pub(crate) fn request_root_exit(&self, status: i32) {
        let capacity = {
            let slots = self.slots.lock();
            for process in slots.iter().filter_map(|slot| slot.process.as_ref()) {
                if 0 < process.members {
                    process.group.signals.mark_exit(status);
                }
            }
            slots.len()
        };
        for index in 0..capacity {
            let group = {
                let slots = self.slots.lock();
                slots[index]
                    .process
                    .as_ref()
                    .filter(|process| 0 < process.members)
                    .map(|process| process.group.clone())
            };
            if let Some(group) = group {
                group.signals.notify();
            }
        }
    }

    /// 已确认停止的成员在独立控制源停驻，完成计数不由普通Blocked推导。
    pub(crate) fn control_waiters(&self) -> &WaitQueue {
        &self.control_waiters
    }

    /// 真实成员在合法安全点确认组控制，末个停止确认才发表父事件。
    pub(crate) fn acknowledge_control(&self, identity: &UserIdentity) -> SignalControl {
        let (control, event) = {
            let mut slots = self.slots.lock();
            let thread = slots[identity.tid() - 1]
                .thread
                .as_ref()
                .expect("unpublished member acknowledged signal control");
            assert!(Arc::ptr_eq(&thread.group, &identity.group));
            let (control, stopped) = thread.signals.acknowledge_control();
            let event = stopped.map(|signum| child_event(&identity.group, 5, signum as i32));
            if let Some(event) = event {
                slots[identity.tgid() - 1]
                    .process
                    .as_mut()
                    .expect("stopping member has no process")
                    .control_event = Some(event);
            }
            (control, event)
        };
        if let Some(event) = event {
            self.notify_parent(&identity.group, event);
            self.waiters.wake_all();
        }
        control
    }

    /// 同锁筛选全部亲缘、观察及唯一消费；WNOWAIT保留事件且不计入父的子CPU账。
    pub(crate) fn try_wait_event(
        &self,
        parent: &ThreadGroup,
        selector: &WaitSelector,
        options: u32,
    ) -> Result<Option<ChildEvent>, ProcessError> {
        const NOWAIT: u32 = 0x0100_0000;
        const WALL: u32 = 0x4000_0000;
        const WCLONE: u32 = 0x8000_0000;
        let mut retired = None;
        let event = {
            let mut slots = self.slots.lock();
            let parent_number = self
                .live_parent(&slots, parent)
                .map_err(|_| ProcessError::NoChild)?;
            let mut eligible = false;
            let mut selected = None;
            for index in 0..slots.len() {
                let Some(process) = slots[index].process.as_ref() else {
                    continue;
                };
                if process.parent != Some(parent_number) {
                    continue;
                }
                let matches = match selector {
                    WaitSelector::Pid(pid) => Arc::ptr_eq(&process.group.pid, pid),
                    WaitSelector::Group(pgid) => {
                        Arc::ptr_eq(&process.group.relations.lock().pgid, pgid)
                    }
                    WaitSelector::Any => true,
                };
                let clone = process.group.relations.lock().exit_signal != 17;
                if !matches || (options & WALL == 0 && clone != (options & WCLONE != 0)) {
                    continue;
                }
                eligible = true;
                if options & 4 != 0 {
                    if let Some(exit) = process.exit {
                        selected = Some(exit_child_event(&process.group, exit.status));
                        if options & NOWAIT == 0 {
                            parent.time.add_reaped_child(&process.group.time);
                            retired = Some(reap_child(&mut slots, index));
                        }
                        break;
                    }
                }
                let control = process.control_event.filter(|event| {
                    (event.code == 5 && options & 2 != 0) || (event.code == 6 && options & 8 != 0)
                });
                if let Some(event) = control {
                    selected = Some(event);
                    if options & NOWAIT == 0 {
                        slots[index]
                            .process
                            .as_mut()
                            .expect("selected process disappeared")
                            .control_event = None;
                    }
                    break;
                }
            }
            if !eligible {
                return Err(ProcessError::NoChild);
            }
            selected
        };
        if retired.is_some() {
            self.waiters.wake_all();
        }
        drop(retired);
        Ok(event)
    }

    /// 活父仍属于原亲缘组时返回其TGID；内核根回收者没有用户PID。
    pub(crate) fn parent_pid(&self, identity: &UserIdentity) -> usize {
        let slots = self.slots.lock();
        if !ptr::eq(self, identity.graph.as_ref()) {
            return 0;
        }
        slots
            .get(identity.tgid() - 1)
            .and_then(|slot| slot.process.as_ref())
            .filter(|process| Arc::ptr_eq(&process.group, &identity.group))
            .and_then(|process| process.parent)
            .and_then(|pid| slots[pid - 1].process.as_ref())
            .filter(|parent| parent.members != 0)
            .map_or(0, |parent| parent.group.tgid())
    }

    /// pid为零查询本组，否则允许查询仍发表的TID或僵尸组首。
    pub(crate) fn get_pgid(
        &self,
        caller: &UserIdentity,
        pid: usize,
    ) -> Result<usize, ProcessError> {
        let slots = self.slots.lock();
        let group = self.identity_target(&slots, caller, pid)?;
        Ok(group.pgid())
    }

    /// 查询有限身份域的会话角色，零选择调用者原组。
    pub(crate) fn get_sid(&self, caller: &UserIdentity, pid: usize) -> Result<usize, ProcessError> {
        let slots = self.slots.lock();
        let group = self.identity_target(&slots, caller, pid)?;
        Ok(group.sid())
    }

    /// 关系锁内验证调用者原组，数字查询只命中当前实际发表的身份。
    fn identity_target<'a>(
        &self,
        slots: &'a [Slot],
        caller: &UserIdentity,
        pid: usize,
    ) -> Result<&'a ThreadGroup, ProcessError> {
        self.live_parent(slots, &caller.group)
            .map_err(|_| ProcessError::NoProcess)?;
        let pid = if pid == 0 { caller.tgid() } else { pid };
        let slot = slots.get(pid - 1).ok_or(ProcessError::NoProcess)?;
        slot.process
            .as_ref()
            .map(|process| process.group.as_ref())
            .or_else(|| slot.thread.as_ref().map(|thread| thread.group.as_ref()))
            .ok_or(ProcessError::NoProcess)
    }

    /// 只允许本组或同会话的直接子改变PGID；已exec子、会话首和跨会话加入被拒绝。
    pub(crate) fn set_pgid(
        &self,
        caller: &UserIdentity,
        pid: usize,
        pgid: usize,
    ) -> Result<(), ProcessError> {
        if pid > i32::MAX as usize || pgid > i32::MAX as usize {
            return Err(ProcessError::InvalidArgument);
        }
        let released = {
            let mut slots = self.slots.lock();
            self.live_parent(&slots, &caller.group)
                .map_err(|_| ProcessError::NoProcess)?;
            let pid = if pid == 0 { caller.tgid() } else { pid };
            let pgid = if pgid == 0 { pid } else { pgid };
            let index = pid - 1;
            let slot = slots.get(index).ok_or(ProcessError::NoProcess)?;
            let process = match slot.process.as_ref() {
                Some(process) => process,
                None if slot.thread.is_some() => return Err(ProcessError::InvalidArgument),
                None => return Err(ProcessError::NoProcess),
            };
            if process.members == 0 {
                return Err(ProcessError::NoProcess);
            }
            let child = process.parent == Some(caller.tgid());
            if pid != caller.tgid() && !child {
                return Err(ProcessError::NoProcess);
            }
            let group = process.group.clone();
            let caller_sid = caller.group.relations.lock().sid.clone();
            if child {
                if !Arc::ptr_eq(&group.relations.lock().sid, &caller_sid) {
                    return Err(ProcessError::Permission);
                }
                if process.did_exec || process.exec_owner.is_some() {
                    return Err(ProcessError::Access);
                }
            }
            if Arc::ptr_eq(&group.relations.lock().sid, &group.pid) {
                return Err(ProcessError::Permission);
            }
            let new_pgid = if pgid == pid {
                group.pid.clone()
            } else {
                let slot = slots.get(pgid - 1).ok_or(ProcessError::Permission)?;
                let target = slot
                    .pid
                    .as_ref()
                    .filter(|_| slot.roles[PGID] != 0)
                    .ok_or(ProcessError::Permission)?;
                let same_session =
                    slots
                        .iter()
                        .filter_map(|slot| slot.process.as_ref())
                        .any(|process| {
                            let relations = process.group.relations.lock();
                            Arc::ptr_eq(&relations.pgid, target)
                                && Arc::ptr_eq(&relations.sid, &caller_sid)
                        });
                if !same_session {
                    return Err(ProcessError::Permission);
                }
                target.clone()
            };
            let mut relations = group.relations.lock();
            if Arc::ptr_eq(&relations.pgid, &new_pgid) {
                return Ok(());
            }
            attach_role(&mut slots, &new_pgid, PGID);
            let old = core::mem::replace(&mut relations.pgid, new_pgid);
            let released = detach_role(&mut slots, &old, PGID);
            (old, released)
        };
        drop(released);
        Ok(())
    }

    /// 当前组不是会话首且本TGID无外部PGID角色时建立新会话，不实现TTY扩展。
    pub(crate) fn create_session(&self, caller: &UserIdentity) -> Result<usize, ProcessError> {
        let released = {
            let mut slots = self.slots.lock();
            self.live_parent(&slots, &caller.group)
                .map_err(|_| ProcessError::NoProcess)?;
            let group = &caller.group;
            let mut relations = group.relations.lock();
            if Arc::ptr_eq(&relations.sid, &group.pid) || slots[caller.tgid() - 1].roles[PGID] != 0
            {
                return Err(ProcessError::Permission);
            }
            attach_role(&mut slots, &group.pid, PGID);
            attach_role(&mut slots, &group.pid, SID);
            let old_pgid = core::mem::replace(&mut relations.pgid, group.pid.clone());
            let old_sid = core::mem::replace(&mut relations.sid, group.pid.clone());
            let pgid = detach_role(&mut slots, &old_pgid, PGID);
            let sid = detach_role(&mut slots, &old_sid, SID);
            (old_pgid, old_sid, pgid, sid)
        };
        drop(released);
        Ok(caller.tgid())
    }

    /// kill按真实进程、进程组或全域选目标，零信号只执行存在性与权限检查。
    pub(crate) fn send_signal(
        &self,
        sender: &UserIdentity,
        pid: i32,
        signum: u8,
    ) -> Result<(), SignalError> {
        if 64 < signum {
            return Err(SignalError::InvalidSignal);
        }
        let info = SignalInfo {
            signum,
            errno: 0,
            code: 0,
            payload: SignalPayload::Sender {
                pid: sender.tgid(),
                uid: sender.group.uid,
                value: 0,
            },
        };
        let capacity = self.slots.lock().len();
        let mut found = false;
        let mut delivered = false;
        let mut error = SignalError::NoProcess;
        for index in 0..capacity {
            let group = {
                let slots = self.slots.lock();
                slots[index]
                    .process
                    .as_ref()
                    .filter(|process| {
                        let target = &process.group;
                        match pid {
                            1.. => target.tgid() == pid as usize,
                            0 => target.pgid() == sender.group.pgid(),
                            -1 => target.tgid() != 1 && target.tgid() != sender.tgid(),
                            _ => target.pgid() == pid.unsigned_abs() as usize,
                        }
                    })
                    .map(|process| process.group.clone())
            };
            let Some(group) = group else {
                continue;
            };
            found = true;
            if !signal_permission(sender, &group, signum) {
                error = SignalError::Permission;
                continue;
            }
            if signum == 0 {
                delivered = true;
                continue;
            }
            match self.deliver_group(&group, info, false) {
                Ok(()) => delivered = true,
                Err(failure) => error = failure,
            }
        }
        if delivered {
            Ok(())
        } else if found {
            Err(error)
        } else {
            Err(SignalError::NoProcess)
        }
    }

    /// tkill与tgkill共用真实TID查找，tgid条件在同一关系快照中核对，零信号不递送。
    pub(crate) fn send_thread_signal(
        &self,
        sender: &UserIdentity,
        tid: usize,
        tgid: Option<usize>,
        signum: u8,
    ) -> Result<(), SignalError> {
        if tid == 0 || tgid == Some(0) || 64 < signum {
            return Err(SignalError::InvalidSignal);
        }
        let info = SignalInfo {
            signum,
            errno: 0,
            code: -6,
            payload: SignalPayload::Sender {
                pid: sender.tgid(),
                uid: sender.group.uid,
                value: 0,
            },
        };
        self.deliver_thread(sender, tid, tgid, info, false)
    }

    /// queueinfo只寻具体进程，外部发送者不得伪造非负kernel code或SI_TKILL。
    pub(crate) fn send_queue(
        &self,
        sender: &UserIdentity,
        pid: usize,
        info: SignalInfo,
    ) -> Result<(), SignalError> {
        if pid == 0 || 64 < info.signum {
            return Err(SignalError::InvalidSignal);
        }
        if (0 <= info.code || info.code == -6) && sender.tid() != pid {
            return Err(SignalError::Permission);
        }
        let group = self.group_target(pid)?;
        if !signal_permission(sender, &group, info.signum) {
            return Err(SignalError::Permission);
        }
        if info.signum == 0 {
            return Ok(());
        }
        self.deliver_group(&group, info, true)
    }

    /// tgsigqueueinfo核对真实组与成员后送private FIFO，队列满保留旧实例并返回QueueFull。
    pub(crate) fn send_thread_queue(
        &self,
        sender: &UserIdentity,
        tid: usize,
        tgid: usize,
        info: SignalInfo,
    ) -> Result<(), SignalError> {
        if tid == 0 || tgid == 0 || 64 < info.signum {
            return Err(SignalError::InvalidSignal);
        }
        if (0 <= info.code || info.code == -6) && sender.tid() != tid {
            return Err(SignalError::Permission);
        }
        self.deliver_thread(sender, tid, Some(tgid), info, true)
    }

    /// 内核控制者通过真实组身份发送普通信号，所有kick与uaccess仍在图和siglock之外。
    pub(crate) fn send_kernel(&self, pid: usize, signum: u8) -> Result<(), SignalError> {
        if signum == 0 || 64 < signum {
            return Err(SignalError::InvalidSignal);
        }
        let group = self.group_target(pid)?;
        self.deliver_group(&group, SignalInfo::kernel(signum), false)
    }

    /// 在关系锁中取得稳定组拥有，退出组仍可被kill0探测但发送不会重新建立成员。
    fn group_target(&self, pid: usize) -> Result<Arc<ThreadGroup>, SignalError> {
        let slots = self.slots.lock();
        slots
            .get(pid.checked_sub(1).ok_or(SignalError::NoProcess)?)
            .and_then(|slot| slot.process.as_ref())
            .map(|process| process.group.clone())
            .ok_or(SignalError::NoProcess)
    }

    /// 同一关系事务保护目标、控制与动作，锁序固定Graph→siglock；最后拥有和通知在锁外。
    fn deliver_group(
        &self,
        group: &Arc<ThreadGroup>,
        info: SignalInfo,
        queued: bool,
    ) -> Result<(), SignalError> {
        let event = {
            let mut slots = self.slots.lock();
            let actual = slots[group.tgid() - 1]
                .process
                .as_ref()
                .ok_or(SignalError::NoProcess)?;
            if !Arc::ptr_eq(&actual.group, group) {
                return Err(SignalError::NoProcess);
            }
            let continued = group.signals.deliver(
                info,
                None,
                queued,
                slots
                    .iter()
                    .filter_map(|slot| slot.thread.as_ref())
                    .filter(|thread| Arc::ptr_eq(&thread.group, group))
                    .map(|thread| &thread.signals),
            )?;
            if continued {
                let event = child_event(group, 6, 18);
                slots[group.tgid() - 1]
                    .process
                    .as_mut()
                    .expect("continuing group disappeared")
                    .control_event = Some(event);
                Some(event)
            } else {
                None
            }
        };
        group.signals.notify();
        if let Some(event) = event {
            self.notify_parent(group, event);
            self.waiters.wake_all();
        }
        Ok(())
    }

    /// 定时IRQ与父事件由原SignalGroup弱图发送，同号重用必须核对原信号拥有。
    pub(super) fn send_group_info(
        &self,
        pid: usize,
        source: &SignalGroup,
        info: SignalInfo,
    ) -> Result<(), SignalError> {
        let group = self.group_target(pid)?;
        if !ptr::eq(group.signals.as_ref(), source) {
            return Err(SignalError::NoProcess);
        }
        self.deliver_group(&group, info, false)
    }

    /// private递送在Graph→siglock事务中完成，通知与弱Task升级不占任一短锁。
    fn deliver_thread(
        &self,
        sender: &UserIdentity,
        tid: usize,
        tgid: Option<usize>,
        info: SignalInfo,
        queued: bool,
    ) -> Result<(), SignalError> {
        let (group, signals, user, event) = {
            let mut slots = self.slots.lock();
            let thread = slots
                .get(tid.checked_sub(1).ok_or(SignalError::NoProcess)?)
                .and_then(|slot| slot.thread.as_ref())
                .ok_or(SignalError::NoProcess)?;
            if tgid.is_some_and(|tgid| thread.group.tgid() != tgid) {
                return Err(SignalError::NoProcess);
            }
            if !signal_permission(sender, &thread.group, info.signum) {
                return Err(SignalError::Permission);
            }
            if info.signum == 0 {
                return Ok(());
            }
            let group = thread.group.clone();
            let signals = thread.signals.clone();
            let user = thread.user.clone();
            let continued = group.signals.deliver(
                info,
                Some(&signals),
                queued,
                slots
                    .iter()
                    .filter_map(|slot| slot.thread.as_ref())
                    .filter(|thread| Arc::ptr_eq(&thread.group, &group))
                    .map(|thread| &thread.signals),
            )?;
            let event = if continued {
                let event = child_event(&group, 6, 18);
                slots[group.tgid() - 1]
                    .process
                    .as_mut()
                    .expect("continuing group disappeared")
                    .control_event = Some(event);
                Some(event)
            } else {
                None
            };
            (group, signals, user, event)
        };
        signals.notify_wait();
        if let Some(user) = user.upgrade() {
            user.kick();
        }
        if info.signum == 18 || group.signals.exit_request().is_some() {
            group.signals.notify();
        }
        if let Some(event) = event {
            self.notify_parent(&group, event);
            self.waiters.wake_all();
        }
        Ok(())
    }

    /// rt_sigaction替换和当前真实组全部pending清理同事务完成，避免裸动作更新绕开组责任。
    pub(crate) fn update_action(
        &self,
        identity: &UserIdentity,
        signal: usize,
        action: Option<SignalAction>,
    ) -> Result<SignalAction, SignalError> {
        let previous = {
            let slots = self.slots.lock();
            let process = slots[identity.tgid() - 1]
                .process
                .as_ref()
                .ok_or(SignalError::NoProcess)?;
            if !Arc::ptr_eq(&process.group, &identity.group) {
                return Err(SignalError::NoProcess);
            }
            identity.group.signals.update_action(
                signal,
                action,
                slots
                    .iter()
                    .filter_map(|slot| slot.thread.as_ref())
                    .filter(|thread| Arc::ptr_eq(&thread.group, &identity.group))
                    .map(|thread| &thread.signals),
            )?
        };
        identity.group.signals.notify();
        Ok(previous)
    }

    /// 逐个快照真实成员后放锁通知，无临时Vec分配；原组对象阻止同号新组接受旧提醒。
    pub(super) fn notify_group(&self, tgid: usize, source: &SignalGroup) {
        let capacity = self.slots.lock().len();
        for index in 0..capacity {
            let member = {
                let slots = self.slots.lock();
                slots[index]
                    .thread
                    .as_ref()
                    .filter(|thread| {
                        thread.group.tgid() == tgid
                            && ptr::eq(thread.group.signals.as_ref(), source)
                    })
                    .map(|thread| (thread.signals.clone(), thread.user.clone()))
            };
            if let Some((signals, user)) = member {
                signals.notify_wait();
                if let Some(user) = user.upgrade() {
                    user.kick();
                }
            }
        }
        self.control_waiters.wake_all();
    }

    /// 父关系快照只在图锁中取得，SIGCHLD编码、发送、等待通知全部在锁外。
    fn notify_parent(&self, group: &ThreadGroup, event: ChildEvent) {
        let parent = {
            let slots = self.slots.lock();
            let process = slots[group.tgid() - 1].process.as_ref();
            process
                .filter(|process| ptr::eq(process.group.as_ref(), group))
                .and_then(|process| process.parent)
                .and_then(|pid| slots[pid - 1].process.as_ref())
                .map(|process| process.group.clone())
        };
        send_parent_event(parent.as_deref(), group, event);
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
            || process.group.signals.exit_request().is_some()
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
        let mut old_relations = None;
        {
            let mut slots = graph.slots.lock();
            let leader = self.identity.tid() == self.identity.tgid();
            let parent = if leader {
                match self.parent.as_ref() {
                    Some(parent) => {
                        let number = graph.live_parent(&slots, parent)?;
                        let inherited = parent.relations.lock();
                        let mut relations = self.identity.group.relations.lock();
                        old_relations = Some((
                            core::mem::replace(&mut relations.pgid, inherited.pgid.clone()),
                            core::mem::replace(&mut relations.sid, inherited.sid.clone()),
                        ));
                        Some(number)
                    }
                    None => None,
                }
            } else {
                graph.live_parent(&slots, &self.identity.group)?;
                if slots[self.identity.tgid() - 1]
                    .process
                    .as_ref()
                    .expect("live group disappeared")
                    .exec_owner
                    .is_some()
                {
                    return Err(ProcessError::Busy);
                }
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
            let tid = self.identity.tid_object();
            slots[index].pid = Some(tid.clone());
            attach_role(&mut slots, &tid, TID);
            slots[index].thread = Some(ThreadEntry {
                user,
                group: self.identity.group.clone(),
                execution,
                signals: self.identity.signals.clone(),
            });
            self.identity.group.signals.add_member();
            if leader {
                attach_role(&mut slots, &self.identity.group.pid, TGID);
                let relations = self.identity.group.relations.lock();
                attach_role(&mut slots, &relations.pgid, PGID);
                attach_role(&mut slots, &relations.sid, SID);
                slots[index].process = Some(ProcessEntry {
                    group: self.identity.group.clone(),
                    parent,
                    members: 1,
                    leader_task_id: task_id,
                    leader_status: None,
                    exec_owner: None,
                    did_exec: false,
                    user_interrupts: 0,
                    user_timer_interrupts: 0,
                    completed: None,
                    exit: None,
                    control_event: None,
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
        drop(old_relations);
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

/// 单一不可变uid凭据满足同uid或root权限，CONT另允许同会话的真实组成员。
fn signal_permission(sender: &UserIdentity, target: &ThreadGroup, signum: u8) -> bool {
    sender.group.uid == 0
        || sender.group.uid == target.uid
        || (signum == 18 && sender.group.sid() == target.sid())
}

/// 把普通Linux退出状态转成waitid字段，不把编码后的status误作si_status。
fn exit_child_event(group: &ThreadGroup, status: i32) -> ChildEvent {
    let signal = status & 0x7f;
    child_event(
        group,
        if signal == 0 {
            1
        } else if status & 0x80 != 0 {
            3
        } else {
            2
        },
        if signal == 0 {
            (status >> 8) & 0xff
        } else {
            signal
        },
    )
}

/// 事件产生时快照真实组账与已消费后代账，不依赖诊断引用或线程数估算。
fn child_event(group: &ThreadGroup, code: i32, status: i32) -> ChildEvent {
    let own = group.time.snapshot();
    let descendants = group.time.children();
    ChildEvent {
        pid: group.tgid(),
        uid: group.uid,
        code,
        status,
        user_ns: own
            .user_ns
            .checked_add(descendants.user_ns)
            .expect("child user time overflow"),
        kernel_ns: own
            .kernel_ns
            .checked_add(descendants.kernel_ns)
            .expect("child kernel time overflow"),
    }
}

/// 图锁内唯一移走退出进程并解绑全部角色，移出的小拥有在锁外释放或join。
fn reap_child(slots: &mut [Slot], index: usize) -> CollectedChild {
    let retired = slots[index]
        .process
        .take()
        .expect("wait process disappeared");
    assert_eq!(retired.members, 0, "reaping a live group");
    assert!(slots[index].thread.is_none(), "reaping a live leader");
    let event = retired.exit.expect("reaping without exit event");
    let released = {
        let relations = retired.group.relations.lock();
        [
            detach_role(slots, &retired.group.pid, TGID),
            detach_role(slots, &relations.pgid, PGID),
            detach_role(slots, &relations.sid, SID),
        ]
    };
    CollectedChild {
        event,
        process: Some(retired),
        _released: released,
    }
}

/// 父拥有已在图锁中稳定取得，退出信号与停止/继续通知各按事件语义锁外发送。
fn send_parent_event(parent: Option<&ThreadGroup>, group: &ThreadGroup, event: ChildEvent) {
    if let Some(parent) = parent {
        let signum = if matches!(event.code, 1..=3) {
            group.relations.lock().exit_signal
        } else {
            17
        };
        if signum == 0 {
            return;
        }
        parent.signals.send_child(SignalInfo {
            signum,
            errno: 0,
            code: event.code,
            payload: SignalPayload::Child {
                pid: event.pid,
                uid: event.uid,
                status: event.status,
                user_ticks: event.user_ns / (1_000_000_000 / USER_HZ),
                kernel_ticks: event.kernel_ns / (1_000_000_000 / USER_HZ),
            },
        });
    }
}
