//! 稳定定时器节点的侵入式红黑树；所有访问由所属 TimerBase 锁串行化。

use core::ptr::{addr_of_mut, null};

use super::Timer;
use crate::time::MonotonicInstant;

/// 节点内嵌的树链，不拥有定时器，也不记录生命周期状态。
pub(super) struct Links {
    parent: *const Timer,
    left: *const Timer,
    right: *const Timer,
    red: bool,
}

impl Links {
    /// 构造尚未接入树的空链。
    pub(super) const fn new() -> Self {
        Self {
            parent: null(),
            left: null(),
            right: null(),
            red: false,
        }
    }
}

/// 按绝对期限排序并缓存最左节点的树，不分配或释放节点。
pub(super) struct Queue {
    root: *const Timer,
    first: *const Timer,
}

impl Queue {
    /// 构造空树。
    pub(super) const fn new() -> Self {
        Self {
            root: null(),
            first: null(),
        }
    }

    /// 返回缓存首节点的期限；调用者仍须持有所属 base 锁。
    pub(super) fn first(&self) -> Option<MonotonicInstant> {
        if self.first.is_null() {
            None
        } else {
            // SAFETY: 只有 insert 能登记指针，其契约保证节点稳定存活、期限不变；base 锁串行化读取。
            Some(unsafe { expires(self.first) })
        }
    }

    /// 接入未登记节点；相同期限向右插入，旋转后两侧均允许相同期限。
    ///
    /// # Safety
    /// 必须持有所属 base 锁；timer 非空、地址稳定、未在任何树中。
    /// 节点及其期限须保持有效且不变，直到 remove 或 pop_due 摘除。
    pub(super) unsafe fn insert(&mut self, timer: *const Timer) {
        // SAFETY: 调用者保证节点有效和独占树访问；遍历只访问此前登记的稳定节点。
        unsafe {
            let expiry = expires(timer);
            let mut parent = null();
            let mut cursor = self.root;
            let mut left = true;
            let mut leftmost = true;
            while !cursor.is_null() {
                parent = cursor;
                left = expiry < expires(cursor);
                if !left {
                    leftmost = false;
                }
                cursor = child(cursor, left);
            }
            *links(timer) = Links {
                parent,
                left: null(),
                right: null(),
                red: true,
            };
            if parent.is_null() {
                self.root = timer;
            } else {
                set_child(parent, left, timer);
            }
            if leftmost {
                self.first = timer;
            }
            self.fix_insert(timer);
        }
    }

    /// 直接摘除已知成员，不搜索期限或修改生命周期状态。
    ///
    /// # Safety
    /// 必须持有所属 base 锁，timer 必须是本树仍稳定存活的真实成员。
    pub(super) unsafe fn remove(&mut self, timer: *const Timer) {
        // SAFETY: 成员及其父子均由本树登记且有效；锁排除观察旋转与移植的中间状态。
        unsafe {
            if self.first == timer {
                self.first = successor(timer);
            }
            let left = child(timer, true);
            let right = child(timer, false);
            let replacement;
            let replacement_parent;
            let removed_red;
            if left.is_null() || right.is_null() {
                replacement = if left.is_null() { right } else { left };
                replacement_parent = (*links(timer)).parent;
                removed_red = is_red(timer);
                self.replace(timer, replacement);
            } else {
                // 双子节点只移植后继的树链，绝不复制 Timer、期限或拥有标记。
                let next = minimum(right);
                removed_red = is_red(next);
                replacement = child(next, false);
                if (*links(next)).parent == timer {
                    replacement_parent = next;
                    if !replacement.is_null() {
                        (*links(replacement)).parent = next;
                    }
                } else {
                    replacement_parent = (*links(next)).parent;
                    self.replace(next, replacement);
                    set_child(next, false, right);
                    (*links(right)).parent = next;
                }
                self.replace(timer, next);
                set_child(next, true, left);
                (*links(left)).parent = next;
                (*links(next)).red = is_red(timer);
            }
            if !removed_red {
                self.fix_remove(replacement, replacement_parent);
            }
            *links(timer) = Links::new();
        }
    }

    /// 摘取一个不晚于 now 的节点，未到期或空树返回 None。
    ///
    /// # Safety
    /// 必须持有所属 base 锁，并维持所有已登记节点的稳定存活和期限不变。
    pub(super) unsafe fn pop_due(&mut self, now: MonotonicInstant) -> Option<*const Timer> {
        let timer = self.first;
        // SAFETY: 缓存指向真实成员；调用者保证锁和登记节点的有效期。
        unsafe {
            if timer.is_null() || now < expires(timer) {
                None
            } else {
                self.remove(timer);
                Some(timer)
            }
        }
    }

    /// 用 new 替换 old 的父链接或根链接，同时修正 new 的父指针。
    ///
    /// # Safety
    /// 独占本树；old 是有效成员，new 为空或有效节点，调用者负责其子链。
    unsafe fn replace(&mut self, old: *const Timer, new: *const Timer) {
        // SAFETY: old 的父节点来自有效树链；new 非空时由调用者保证可写。
        unsafe {
            let parent = (*links(old)).parent;
            if parent.is_null() {
                self.root = new;
            } else {
                set_child(parent, child(parent, true) == old, new);
            }
            if !new.is_null() {
                (*links(new)).parent = parent;
            }
        }
    }

    /// 提升指定一侧的子节点；promote_left 为真是右旋，否则是左旋。
    ///
    /// # Safety
    /// 独占本树；old 为有效成员且待提升的子节点非空，所有链接有效。
    unsafe fn rotate(&mut self, old: *const Timer, promote_left: bool) {
        // SAFETY: 非空子节点与中间子树均来自有效树链，锁排除中间结构被访问。
        unsafe {
            let new = child(old, promote_left);
            let middle = child(new, !promote_left);
            set_child(old, promote_left, middle);
            if !middle.is_null() {
                (*links(middle)).parent = old;
            }
            self.replace(old, new);
            set_child(new, !promote_left, old);
            (*links(old)).parent = new;
        }
    }

    /// 修复新红节点引起的红红相邻，最后将根染黑。
    ///
    /// # Safety
    /// 独占本树；node 是刚接入的红叶，原树满足全部红黑不变量。
    unsafe fn fix_insert(&mut self, mut node: *const Timer) {
        // SAFETY: 循环沿有效父链推进；红父不可能为黑根，因此其祖父存在。
        unsafe {
            let mut parent = (*links(node)).parent;
            while is_red(parent) {
                let grandparent = (*links(parent)).parent;
                let parent_left = child(grandparent, true) == parent;
                let uncle = child(grandparent, !parent_left);
                if is_red(uncle) {
                    set_red(parent, false);
                    set_red(uncle, false);
                    set_red(grandparent, true);
                    node = grandparent;
                    parent = (*links(node)).parent;
                    continue;
                }
                if child(parent, !parent_left) == node {
                    self.rotate(parent, !parent_left);
                    node = parent;
                    parent = (*links(node)).parent;
                }
                set_red(parent, false);
                set_red(grandparent, true);
                self.rotate(grandparent, parent_left);
                break;
            }
            set_red(self.root, false);
        }
    }

    /// 修复摘除黑节点后的一层黑高欠缺；空叶以显式 parent 定位。
    ///
    /// # Safety
    /// 独占本树；仅 node 路径缺一层黑高，parent 为其真实父节点，根可为空。
    unsafe fn fix_remove(&mut self, mut node: *const Timer, mut parent: *const Timer) {
        // SAFETY: 原红黑性质保证非根欠缺路径具有非空兄弟；旋转后重新读取实际兄弟。
        unsafe {
            while node != self.root && !is_red(node) {
                let node_left = child(parent, true) == node;
                let mut sibling = child(parent, !node_left);
                debug_assert!(!sibling.is_null());
                if is_red(sibling) {
                    set_red(sibling, false);
                    set_red(parent, true);
                    self.rotate(parent, !node_left);
                    sibling = child(parent, !node_left);
                }
                let near = child(sibling, node_left);
                let far = child(sibling, !node_left);
                if !is_red(near) && !is_red(far) {
                    set_red(sibling, true);
                    node = parent;
                    parent = (*links(node)).parent;
                    continue;
                }
                if !is_red(far) {
                    set_red(near, false);
                    set_red(sibling, true);
                    self.rotate(sibling, node_left);
                    sibling = child(parent, !node_left);
                }
                set_red(sibling, is_red(parent));
                set_red(parent, false);
                set_red(child(sibling, !node_left), false);
                self.rotate(parent, !node_left);
                node = self.root;
            }
            set_red(node, false);
        }
    }

    /// 核验真实成员、黑高、全局排序、父链、颜色与首节点缓存。
    ///
    /// # Safety
    /// 必须持有所属 base 锁；所有登记指针均须有效且稳定存活。
    #[cfg(feature = "ktest")]
    pub(super) unsafe fn validate(&self) -> (usize, usize) {
        // SAFETY: 调用者保证节点有效；递归先核父链，以拒绝回边和重复子树。
        unsafe {
            if self.root.is_null() {
                assert!(self.first.is_null());
                return (0, 1);
            }
            assert!(!self.first.is_null());
            assert!(!is_red(self.root));
            let result = validate_subtree(self.root, null(), None, None);
            assert!(self.first == minimum(self.root));
            result
        }
    }
}

/// 取得节点内部 rb 原始地址，不建立覆盖整个 Timer 或 Node 的可变借用。
///
/// # Safety
/// timer 非空且有效，调用者持所属 base 锁；返回地址只能在该临界区内使用。
unsafe fn links(timer: *const Timer) -> *mut Links {
    // SAFETY: UnsafeCell 允许锁保护下操作内嵌树链，addr_of_mut 不构造长期引用。
    unsafe { addr_of_mut!((*(*timer).node.get()).rb) }
}

/// 复制内部绝对期限，不保留 Node 的共享借用。
///
/// # Safety
/// timer 非空且有效，持所属 base 锁，登记期间期限不可修改。
unsafe fn expires(timer: *const Timer) -> MonotonicInstant {
    // SAFETY: 调用者保证存储有效和读写串行化，此处仅复制 Copy 字段。
    unsafe { (*(*timer).node.get()).expires }
}

/// 读取一侧子链；left 为真读取左子链。
///
/// # Safety
/// timer 非空且有效，并持所属 base 锁。
unsafe fn child(timer: *const Timer, left: bool) -> *const Timer {
    // SAFETY: links 只定位临界区内有效的内嵌树链。
    unsafe {
        if left {
            (*links(timer)).left
        } else {
            (*links(timer)).right
        }
    }
}

/// 修改一侧子链，不隐式修改子节点的父指针。
///
/// # Safety
/// timer 非空且有效，并独占树链；value 为空或稳定有效节点。
unsafe fn set_child(timer: *const Timer, left: bool, value: *const Timer) {
    // SAFETY: 调用者独占有效树链，原始字段写入不会建立重叠可变引用。
    unsafe {
        if left {
            (*links(timer)).left = value;
        } else {
            (*links(timer)).right = value;
        }
    }
}

/// 读取节点颜色，空叶固定为黑色。
///
/// # Safety
/// timer 非空时必须有效，并持所属 base 锁。
unsafe fn is_red(timer: *const Timer) -> bool {
    // SAFETY: 短路排除空指针，非空节点有效且锁保护颜色读取。
    unsafe { !timer.is_null() && (*links(timer)).red }
}

/// 设置非空节点颜色；空叶保持黑色而不写入。
///
/// # Safety
/// timer 非空时必须有效，并独占其树链。
unsafe fn set_red(timer: *const Timer, red: bool) {
    if !timer.is_null() {
        // SAFETY: 非空节点有效，调用者持锁且独占树链颜色写入。
        unsafe { (*links(timer)).red = red };
    }
}

/// 沿真实左子链求非空子树的首节点。
///
/// # Safety
/// node 非空且树链有效、无环，调用者持所属 base 锁。
unsafe fn minimum(mut node: *const Timer) -> *const Timer {
    // SAFETY: 子树节点有效，循环只沿非空左链推进。
    unsafe {
        while !child(node, true).is_null() {
            node = child(node, true);
        }
        node
    }
}

/// 沿右子树或父链求真实中序后继，无后继时返回空指针。
///
/// # Safety
/// node 为有效树成员，父子链完整，调用者持所属 base 锁。
unsafe fn successor(mut node: *const Timer) -> *const Timer {
    // SAFETY: 所有遍历指针来自有效成员的真实父子链。
    unsafe {
        let right = child(node, false);
        if !right.is_null() {
            return minimum(right);
        }
        let mut parent = (*links(node)).parent;
        while !parent.is_null() && child(parent, false) == node {
            node = parent;
            parent = (*links(node)).parent;
        }
        parent
    }
}

/// 在真实子树递归核验包含重复期限的闭区间排序与红黑性质。
///
/// # Safety
/// 所有可达非空指针有效，调用者持所属 base 锁；可为空子树。
#[cfg(feature = "ktest")]
unsafe fn validate_subtree(
    node: *const Timer,
    parent: *const Timer,
    lower: Option<MonotonicInstant>,
    upper: Option<MonotonicInstant>,
) -> (usize, usize) {
    if node.is_null() {
        return (0, 1);
    }
    // SAFETY: 调用者保证可达节点有效；父链核验拒绝祖先回边，异侧相同指针被显式拒绝。
    unsafe {
        assert!((*links(node)).parent == parent);
        let expiry = expires(node);
        if let Some(lower) = lower {
            assert!(lower <= expiry);
        }
        if let Some(upper) = upper {
            assert!(expiry <= upper);
        }
        let left = child(node, true);
        let right = child(node, false);
        assert!(left.is_null() || left != right);
        if is_red(node) {
            assert!(!is_red(left) && !is_red(right));
        }
        let (left_count, left_black) = validate_subtree(left, node, lower, Some(expiry));
        let (right_count, right_black) = validate_subtree(right, node, Some(expiry), upper);
        assert!(left_black == right_black);
        (
            left_count + right_count + 1,
            left_black + usize::from(!is_red(node)),
        )
    }
}
