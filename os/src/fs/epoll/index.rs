//! 稳定事件节点的独立 AVL 投影，旋转只移动原 Arc 链。

use alloc::sync::Arc;
use core::cell::UnsafeCell;

use crate::fs::epoll::{Entry, Key};

type Link = Option<Arc<Entry>>;

struct IndexLinks {
    height: u16,
    left: Link,
    right: Link,
}

/// 该投影只由所属事件树的控制互斥域访问，候选链使用另一块存储。
pub(super) struct Links(UnsafeCell<IndexLinks>);

impl Links {
    /// 未发表节点是高度为一且不拥有子树的叶。
    pub(super) const fn new() -> Self {
        Self(UnsafeCell::new(IndexLinks {
            height: 1,
            left: None,
            right: None,
        }))
    }

    /// 只在所属树共享借用期间读取固定索引投影。
    fn get(&self) -> &IndexLinks {
        // SAFETY: 本模块仅在 Tree 的共享或独占借用期间访问索引；外部入口持控制锁，
        // 源回调和候选链均不访问这一 UnsafeCell，也不取得整个 Entry 的可变借用。
        unsafe { &*self.0.get() }
    }

    /// 独占树操作的凭证限制同一投影的可变借用。
    fn get_mut<'a>(&'a self, _write: &'a mut IndexWrite) -> &'a mut IndexLinks {
        // SAFETY: IndexWrite 仅由独占 Tree 操作建立，旋转只修改该树的节点投影；
        // Arc 读者只观察不可变身份或独立候选字段，没有覆盖本存储的共享借用。
        unsafe { &mut *self.0.get() }
    }

    /// 已退休节点不得通过子链继续拥有其它活 interest。
    pub(super) fn detached(&self) -> bool {
        let links = self.get();
        links.left.is_none() && links.right.is_none()
    }
}

struct IndexWrite;

pub(super) struct Tree {
    root: Link,
    pub len: usize,
}

impl Tree {
    /// 空树不按 fd 值预留空间，容量仅约束真实节点数。
    pub const fn new() -> Self {
        Self { root: None, len: 0 }
    }

    /// 控制域内命中原稳定节点，不复制身份或源登记。
    pub fn get(&self, key: Key) -> Option<&Arc<Entry>> {
        let mut cursor = self.root.as_ref();
        while let Some(node) = cursor {
            if key == node.key {
                return Some(node);
            }
            cursor = if key < node.key {
                node.index.get().left.as_ref()
            } else {
                node.index.get().right.as_ref()
            };
        }
        None
    }

    /// 最左原票供最终释放逐项注销，返回节点仍由树保活。
    pub fn first(&self) -> Option<Arc<Entry>> {
        let mut cursor = self.root.as_ref()?;
        while let Some(left) = cursor.index.get().left.as_ref() {
            cursor = left;
        }
        Some(Arc::clone(cursor))
    }

    /// 发表锁外准备的唯一叶，插入和旋转均不分配。
    pub fn insert(&mut self, node: Arc<Entry>) {
        self.root = Some(insert(self.root.take(), node, &mut IndexWrite));
        self.len += 1;
    }

    /// 摘除原节点及其全部子链，拥有交给控制锁外的退休者。
    pub fn remove(&mut self, key: Key) -> Link {
        let (root, removed) = remove(self.root.take(), key, &mut IndexWrite);
        self.root = root;
        if removed.is_some() {
            self.len -= 1;
        }
        removed
    }
}

impl Drop for Tree {
    /// 事件对象必须先正式撤源及反链，树自身不能暗中替代这个顺序。
    fn drop(&mut self) {
        assert!(
            self.root.is_none() && self.len == 0,
            "epoll interest tree not drained"
        );
    }
}

/// 空子树高度为零，高度读取不访问候选投影。
fn height(link: &Link) -> u16 {
    link.as_ref().map_or(0, |node| node.index.get().height)
}

/// 子链变化后只重算原节点的高度。
fn refresh(node: &Entry, write: &mut IndexWrite) {
    let links = node.index.get_mut(write);
    links.height = 1 + height(&links.left).max(height(&links.right));
}

/// 提升左孩子，固定身份与源、候选票不随旋转改变。
fn rotate_right(root: Arc<Entry>, write: &mut IndexWrite) -> Arc<Entry> {
    let next = root
        .index
        .get_mut(write)
        .left
        .take()
        .expect("AVL right rotation has no child");
    root.index.get_mut(write).left = next.index.get_mut(write).right.take();
    refresh(&root, write);
    next.index.get_mut(write).right = Some(root);
    refresh(&next, write);
    next
}

/// 提升右孩子，只移动已经拥有的子链。
fn rotate_left(root: Arc<Entry>, write: &mut IndexWrite) -> Arc<Entry> {
    let next = root
        .index
        .get_mut(write)
        .right
        .take()
        .expect("AVL left rotation has no child");
    root.index.get_mut(write).right = next.index.get_mut(write).left.take();
    refresh(&root, write);
    next.index.get_mut(write).left = Some(root);
    refresh(&next, write);
    next
}

/// 插入或删除返回路径恢复 AVL 高度差，不替换节点身份。
fn balance(root: Arc<Entry>, write: &mut IndexWrite) -> Arc<Entry> {
    refresh(&root, write);
    if height(&root.index.get().right) + 1 < height(&root.index.get().left) {
        let left = root.index.get().left.as_ref().unwrap();
        if height(&left.index.get().left) < height(&left.index.get().right) {
            let left = root.index.get_mut(write).left.take().unwrap();
            root.index.get_mut(write).left = Some(rotate_left(left, write));
        }
        return rotate_right(root, write);
    }
    if height(&root.index.get().left) + 1 < height(&root.index.get().right) {
        let right = root.index.get().right.as_ref().unwrap();
        if height(&right.index.get().right) < height(&right.index.get().left) {
            let right = root.index.get_mut(write).right.take().unwrap();
            root.index.get_mut(write).right = Some(rotate_right(right, write));
        }
        return rotate_left(root, write);
    }
    root
}

/// 准备叶沿完整身份键插入，重复键须在控制入口已经拒绝。
fn insert(root: Link, node: Arc<Entry>, write: &mut IndexWrite) -> Arc<Entry> {
    let Some(root) = root else {
        return node;
    };
    assert!(root.key != node.key, "duplicate epoll interest key");
    if node.key < root.key {
        let child = root.index.get_mut(write).left.take();
        root.index.get_mut(write).left = Some(insert(child, node, write));
    } else {
        let child = root.index.get_mut(write).right.take();
        root.index.get_mut(write).right = Some(insert(child, node, write));
    }
    balance(root, write)
}

/// 摘出最左原节点，返回的票不再拥有子树。
fn pop_first(root: Arc<Entry>, write: &mut IndexWrite) -> (Link, Arc<Entry>) {
    let Some(left) = root.index.get_mut(write).left.take() else {
        return (root.index.get_mut(write).right.take(), root);
    };
    let (left, removed) = pop_first(left, write);
    root.index.get_mut(write).left = left;
    (Some(balance(root, write)), removed)
}

/// 双子删除移植后继原票，不复制不可变键或源关系。
fn remove(root: Link, key: Key, write: &mut IndexWrite) -> (Link, Link) {
    let Some(root) = root else {
        return (None, None);
    };
    if key < root.key {
        let (left, removed) = remove(root.index.get_mut(write).left.take(), key, write);
        root.index.get_mut(write).left = left;
        return (Some(balance(root, write)), removed);
    }
    if root.key < key {
        let (right, removed) = remove(root.index.get_mut(write).right.take(), key, write);
        root.index.get_mut(write).right = right;
        return (Some(balance(root, write)), removed);
    }
    let left = root.index.get_mut(write).left.take();
    let Some(right) = root.index.get_mut(write).right.take() else {
        return (left, Some(root));
    };
    let (right, next) = pop_first(right, write);
    next.index.get_mut(write).left = left;
    next.index.get_mut(write).right = right;
    (Some(balance(next, write)), Some(root))
}
