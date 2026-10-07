//! 同一页索引使用拥有型 AVL 节点；分配和移出节点的析构由短锁外调用者负责。

use alloc::boxed::Box;

use crate::block::BlockError;
use crate::mm::FolioRef;

pub(super) type Link = Option<Box<Node>>;

pub(super) struct Node {
    pub key: u64,
    pub folio: FolioRef,
    pub to_write: bool,
    height: u16,
    left: Link,
    right: Link,
}

pub(super) struct Tree {
    root: Link,
    pub len: usize,
    pub closed: bool,
}

impl Node {
    /// 在索引锁外准备真实节点；失败时页拥有随调用者的候选一起退出。
    pub fn prepare(key: u64, folio: FolioRef) -> Result<Box<Self>, BlockError> {
        Box::try_new(Self {
            key,
            folio,
            to_write: false,
            height: 1,
            left: None,
            right: None,
        })
        .map_err(|_| BlockError::NoMemory)
    }
}

impl Tree {
    /// 建立空稀疏树，不按文件长度预留槽或节点。
    pub const fn new() -> Self {
        Self {
            root: None,
            len: 0,
            closed: false,
        }
    }

    /// 命中只借用原节点，不分配或改变页身份。
    pub fn get(&self, key: u64) -> Option<&Node> {
        let mut cursor = self.root.as_deref();
        while let Some(node) = cursor {
            if key == node.key {
                return Some(node);
            }
            cursor = if key < node.key {
                node.left.as_deref()
            } else {
                node.right.as_deref()
            };
        }
        None
    }

    /// 返回不小于起点的首节点，范围扫描无需复制整张索引。
    pub fn lower_bound(&self, key: u64) -> Option<&Node> {
        let mut cursor = self.root.as_deref();
        let mut found = None;
        while let Some(node) = cursor {
            if node.key < key {
                cursor = node.right.as_deref();
            } else {
                found = Some(node);
                cursor = node.left.as_deref();
            }
        }
        found
    }

    /// 修改同一个节点的本轮待写选择，不另建脏页表。
    pub fn set_to_write(&mut self, key: u64, selected: bool) {
        let mut cursor = self.root.as_deref_mut();
        while let Some(node) = cursor {
            if key == node.key {
                node.to_write = selected;
                return;
            }
            cursor = if key < node.key {
                node.left.as_deref_mut()
            } else {
                node.right.as_deref_mut()
            };
        }
    }

    /// 接入锁外准备的唯一叶；调用者已在同一锁下排除重复键。
    pub fn insert(&mut self, node: Box<Node>) {
        self.root = Some(insert(self.root.take(), node));
        self.len += 1;
    }

    /// 摘除原节点并返回完整拥有；节点中的页和堆存储不得在索引锁内析构。
    pub fn remove(&mut self, key: u64) -> Link {
        let (root, removed) = remove(self.root.take(), key);
        self.root = root;
        if removed.is_some() {
            self.len -= 1;
        }
        removed
    }

    /// 在关闭新取得以后转移全部节点，调用者在锁外逐节点处理原页。
    pub fn take_all(&mut self) -> Self {
        Self {
            root: self.root.take(),
            len: core::mem::replace(&mut self.len, 0),
            closed: true,
        }
    }

    /// 核验真实 AVL 排序、缓存高度、平衡与节点计数；验收调用者持原索引短锁。
    #[cfg(feature = "ktest")]
    pub fn validate(&self) -> (usize, u16) {
        let result = validate(&self.root, None, None);
        assert_eq!(result.0, self.len);
        result
    }
}

/// 空子树高度为零，真实节点缓存自己的平衡高度。
fn height(link: &Link) -> u16 {
    link.as_ref().map_or(0, |node| node.height)
}

/// 子链变更后仅重算本节点，不遍历未改变的子树。
fn refresh(node: &mut Node) {
    node.height = 1 + height(&node.left).max(height(&node.right));
}

/// 提升左子节点，转移 Box 拥有而不分配或制造侵入指针别名。
fn rotate_right(mut root: Box<Node>) -> Box<Node> {
    let mut next = root
        .left
        .take()
        .expect("AVL right rotation has no left child");
    root.left = next.right.take();
    refresh(&mut root);
    next.right = Some(root);
    refresh(&mut next);
    next
}

/// 提升右子节点，旋转过程中所有节点仍只有一份堆拥有。
fn rotate_left(mut root: Box<Node>) -> Box<Node> {
    let mut next = root
        .right
        .take()
        .expect("AVL left rotation has no right child");
    root.right = next.left.take();
    refresh(&mut root);
    next.left = Some(root);
    refresh(&mut next);
    next
}

/// 单次插入或删除后恢复 AVL 高度差，使稀疏有序访问保持对数深度。
fn balance(mut root: Box<Node>) -> Box<Node> {
    refresh(&mut root);
    if height(&root.right) + 1 < height(&root.left) {
        let left = root.left.as_ref().unwrap();
        if height(&left.left) < height(&left.right) {
            root.left = Some(rotate_left(root.left.take().unwrap()));
        }
        return rotate_right(root);
    }
    if height(&root.left) + 1 < height(&root.right) {
        let right = root.right.as_ref().unwrap();
        if height(&right.right) < height(&right.left) {
            root.right = Some(rotate_right(root.right.take().unwrap()));
        }
        return rotate_left(root);
    }
    root
}

/// 将预备叶沿键次序移入子树，只在返回路径修复高度。
fn insert(root: Link, node: Box<Node>) -> Box<Node> {
    let Some(mut root) = root else {
        return node;
    };
    assert_ne!(root.key, node.key, "duplicate folio index key");
    if node.key < root.key {
        root.left = Some(insert(root.left.take(), node));
    } else {
        root.right = Some(insert(root.right.take(), node));
    }
    balance(root)
}

/// 摘取最左节点用于双子删除；返回的节点已经没有任何子链。
fn pop_first(mut root: Box<Node>) -> (Link, Box<Node>) {
    let Some(left) = root.left.take() else {
        return (root.right.take(), root);
    };
    let (left, removed) = pop_first(left);
    root.left = left;
    (Some(balance(root)), removed)
}

/// 移植后继节点而不复制键、folio 或待写标记，移出的节点交锁外释放。
fn remove(root: Link, key: u64) -> (Link, Link) {
    let Some(mut root) = root else {
        return (None, None);
    };
    if key < root.key {
        let (left, removed) = remove(root.left.take(), key);
        root.left = left;
        return (Some(balance(root)), removed);
    }
    if root.key < key {
        let (right, removed) = remove(root.right.take(), key);
        root.right = right;
        return (Some(balance(root)), removed);
    }
    let left = root.left.take();
    let Some(right) = root.right.take() else {
        return (left, Some(root));
    };
    let (right, mut next) = pop_first(right);
    next.left = left;
    next.right = right;
    (Some(balance(next)), Some(root))
}

/// 递归只沿拥有型子链检查全局键界限，不申请任何辅助测试存储。
#[cfg(feature = "ktest")]
fn validate(link: &Link, low: Option<u64>, high: Option<u64>) -> (usize, u16) {
    let Some(node) = link else {
        return (0, 0);
    };
    assert!(low.is_none_or(|low| low < node.key));
    assert!(high.is_none_or(|high| node.key < high));
    let left = validate(&node.left, low, Some(node.key));
    let right = validate(&node.right, Some(node.key), high);
    assert!(left.1.abs_diff(right.1) <= 1);
    assert_eq!(node.height, 1 + left.1.max(right.1));
    (1 + left.0 + right.0, node.height)
}
