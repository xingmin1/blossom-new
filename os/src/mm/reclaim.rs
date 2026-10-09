//! 普通任务明确物理页与文件候选准备的有限慢路；原页和标准堆入口保持 NOWAIT。

use crate::block::BlockError;
use crate::cache;
use crate::mm::{alloc_pages, OwnedPages};
use crate::task::ReclaimMode;

const SCAN_BUDGET: usize = 64;
const ISOLATE_BUDGET: usize = 16;
const RMAP_BUDGET: usize = 64;

/// 单一请求的实际消费；节点、储备复用和 I/O 完成都不算归还 file 页。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ReclaimStats {
    pub scanned: usize,
    pub isolated: usize,
    pub rmap_checks: usize,
    pub unmapped: usize,
    pub freed_file: usize,
    pub freed_tables: usize,
    pub submitted_bytes: usize,
    pub completed_bytes: usize,
    pub io_error: bool,
}

/// 目标页及中间表页共用原请求预算，来源重查不创建新的无限回收请求。
pub struct ReclaimAttempt {
    stats: ReclaimStats,
    waited: bool,
}

impl ReclaimAttempt {
    /// 只允许普通可睡眠任务准备请求，不改变分配器快路或分配任何控制对象。
    pub(crate) fn new() -> Self {
        crate::sched::assert_switchable();
        Self {
            stats: ReclaimStats::default(),
            waited: false,
        }
    }

    /// 读取本请求真实消费，不把候选数或选写页数冒充设备提交及归页。
    pub(crate) fn stats(&self) -> ReclaimStats {
        self.stats
    }

    /// 每次慢路读取当前任务的限制，调用者后取得 FS 依赖时不能沿用旧 Normal。
    pub(crate) fn mode(&self) -> ReclaimMode {
        ReclaimMode::current()
    }

    /// 同一生产页入口先试一次，实际回收有归页后仅重试一次相同阶和清零要求。
    pub(crate) fn allocate_pages(&mut self, order: usize, zeroed: bool) -> Option<OwnedPages> {
        if let Some(pages) = alloc_pages(order, zeroed) {
            return Some(pages);
        }
        self.reclaim().then(|| alloc_pages(order, zeroed)).flatten()
    }

    /// 私有候选失败已完整退出后调用；仅实际 file 或表页归还准许同源重试。
    pub(crate) fn reclaim(&mut self) -> bool {
        let before = self.stats.freed_file + self.stats.freed_tables;
        cache::reclaim_file(self);
        before < self.stats.freed_file + self.stats.freed_tables
    }

    /// 老化与隔离共用入口总预算，扫描者据此裁剪实际队列有限前缀。
    pub(crate) fn scan_remaining(&self) -> usize {
        SCAN_BUDGET.saturating_sub(self.stats.scanned)
    }

    /// 只消费一次原 LRU 扫描预算，访问老化及弱身份失败也占该预算。
    pub(crate) fn scan(&mut self) -> bool {
        if SCAN_BUDGET <= self.stats.scanned {
            return false;
        }
        self.stats.scanned += 1;
        true
    }

    /// 实际隔离一张页以前消费隔离预算；所有后续分支都交还或退休同票。
    pub(crate) fn isolate(&mut self) -> bool {
        if ISOLATE_BUDGET <= self.stats.isolated {
            return false;
        }
        self.stats.isolated += 1;
        true
    }

    /// 隔离额度结束后不再反复核同一队首；未消费的扫描预算仍可用于有限 active 老化。
    pub(crate) fn can_isolate(&self) -> bool {
        self.stats.isolated < ISOLATE_BUDGET
    }

    /// 每个真实范围核验共用总预算，mkclean 与 unmap 均不能另开一份预算。
    pub(crate) fn rmap(&mut self) -> bool {
        if RMAP_BUDGET <= self.stats.rmap_checks {
            return false;
        }
        self.stats.rmap_checks += 1;
        true
    }

    /// 最多取得一次 WB 值进度等待权；NOFS/NOIO 不请求或等待文件写回。
    pub(crate) fn wait_writeback(&mut self) -> bool {
        if self.waited || self.mode() != ReclaimMode::Normal {
            return false;
        }
        self.waited = true;
        true
    }

    /// 将经过真实 CSD 及拥有者 Drop 的计数合入同请求，I/O 值由原完成账提供。
    pub(crate) fn account(&mut self, stats: ReclaimStats) {
        self.stats.unmapped += stats.unmapped;
        self.stats.freed_file += stats.freed_file;
        self.stats.freed_tables += stats.freed_tables;
        self.stats.submitted_bytes += stats.submitted_bytes;
        self.stats.completed_bytes += stats.completed_bytes;
        self.stats.io_error |= stats.io_error;
    }

    /// 文件页及 Node 私有候选先完整析构，有实际归页时仅重新准备一次同来源。
    pub(crate) fn prepare<T>(
        &mut self,
        mut prepare: impl FnMut() -> Result<T, BlockError>,
    ) -> Result<T, BlockError> {
        match prepare() {
            Err(BlockError::NoMemory) if self.reclaim() => prepare(),
            result => result,
        }
    }
}
