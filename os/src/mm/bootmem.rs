//! 启动阶段按真实 RAM 管理整页区间，元数据只从未保留页取得。

use crate::boot::{BootInfo, Region, MAX_MEMORY_REGIONS, MAX_RESERVED_REGIONS};

pub(crate) const PAGE_SIZE: usize = 4096;
// root 索引一次，每段 RAM 至多一次描述符 backing。
const MAX_METADATA_ALLOCATIONS: usize = 1 + MAX_MEMORY_REGIONS;
pub(crate) const RANGE_CAPACITY: usize =
    MAX_MEMORY_REGIONS + MAX_RESERVED_REGIONS + MAX_METADATA_ALLOCATIONS;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PageRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BootStats {
    pub present_pages: usize,
    pub reserved_pages: usize,
    pub metadata_pages: usize,
    pub free_pages: usize,
}

struct Ranges {
    entries: [PageRange; RANGE_CAPACITY],
    count: usize,
}

impl Ranges {
    /// 创建启动期小型固定容量表，不借用堆存储。
    fn new() -> Self {
        Self {
            entries: [PageRange { start: 0, end: 0 }; RANGE_CAPACITY],
            count: 0,
        }
    }

    /// 仅返回按起点排序、互不相交且不相邻的有效区间。
    fn as_slice(&self) -> &[PageRange] {
        &self.entries[..self.count]
    }

    /// 将区间并入有序集合，容量错误不改变原集合。
    fn insert(&mut self, mut range: PageRange) -> Result<(), &'static str> {
        if range.end <= range.start {
            return Ok(());
        }
        let first = self
            .as_slice()
            .partition_point(|entry| entry.end < range.start);
        let mut last = first;
        while last < self.count && self.entries[last].start <= range.end {
            range.start = range.start.min(self.entries[last].start);
            range.end = range.end.max(self.entries[last].end);
            last += 1;
        }
        if first == last {
            if self.count == RANGE_CAPACITY {
                return Err("boot range capacity exceeded");
            }
            self.entries.copy_within(first..self.count, first + 1);
            self.count += 1;
        } else {
            self.entries.copy_within(last..self.count, first + 1);
            self.count -= last - first - 1;
        }
        self.entries[first] = range;
        Ok(())
    }

    /// 先合并物理字节区间再取整，避免相邻片段共同覆盖的完整页丢失。
    fn from_regions(regions: &[Region], inward: bool) -> Result<Self, &'static str> {
        let mut ranges = Self::new();
        for region in regions {
            if region.end <= region.start {
                return Err("invalid boot physical region");
            }
            ranges.insert(PageRange {
                start: region.start,
                end: region.end,
            })?;
        }
        let mut output = 0;
        for index in 0..ranges.count {
            let region = ranges.entries[index];
            let range = if inward {
                PageRange {
                    start: round_up(region.start)? / PAGE_SIZE,
                    end: region.end / PAGE_SIZE,
                }
            } else {
                PageRange {
                    start: region.start / PAGE_SIZE,
                    end: round_up(region.end)? / PAGE_SIZE,
                }
            };
            if range.start < range.end {
                if 0 < output && range.start <= ranges.entries[output - 1].end {
                    ranges.entries[output - 1].end = ranges.entries[output - 1].end.max(range.end);
                } else {
                    ranges.entries[output] = range;
                    output += 1;
                }
            }
        }
        ranges.count = output;
        Ok(ranges)
    }

    /// 累计互不重叠区间的页数。
    fn pages(&self) -> usize {
        self.as_slice()
            .iter()
            .map(|range| range.end - range.start)
            .sum()
    }
}

/// 地址向上取整时拒绝超出物理地址整数范围。
fn round_up(address: usize) -> Result<usize, &'static str> {
    address
        .checked_add(PAGE_SIZE - 1)
        .map(|value| value & !(PAGE_SIZE - 1))
        .ok_or("boot page alignment overflows")
}

pub(crate) struct BootMemory {
    ram: Ranges,
    reserved: Ranges,
    metadata: Ranges,
    free: Ranges,
    allocation_count: usize,
}

impl BootMemory {
    /// 从不可变固件快照得到整页 RAM，并只扣除保留区与 RAM 的交集。
    pub(crate) fn new(info: &BootInfo) -> Result<Self, &'static str> {
        if MAX_MEMORY_REGIONS < info.memory_count || MAX_RESERVED_REGIONS < info.reserved_count {
            return Err("boot region count exceeds capacity");
        }
        let ram = Ranges::from_regions(&info.memory[..info.memory_count], true)?;
        let reservations = Ranges::from_regions(&info.reserved[..info.reserved_count], false)?;
        let mut memory = Self {
            ram,
            reserved: Ranges::new(),
            metadata: Ranges::new(),
            free: Ranges::new(),
            allocation_count: 0,
        };
        let mut reservation_index = 0;
        for ram in memory.ram.as_slice() {
            let mut cursor = ram.start;
            while reservation_index < reservations.count
                && reservations.entries[reservation_index].end <= cursor
            {
                reservation_index += 1;
            }
            let mut index = reservation_index;
            while index < reservations.count && reservations.entries[index].start < ram.end {
                let reservation = reservations.entries[index];
                let start = reservation.start.max(cursor);
                let end = reservation.end.min(ram.end);
                memory.free.insert(PageRange {
                    start: cursor,
                    end: start,
                })?;
                memory.reserved.insert(PageRange { start, end })?;
                cursor = end;
                if ram.end <= reservation.end {
                    break;
                }
                index += 1;
            }
            memory.free.insert(PageRange {
                start: cursor,
                end: ram.end,
            })?;
            reservation_index = index;
        }
        Ok(memory)
    }

    /// 借用真实整页 RAM 并集，空洞不属于该集合。
    pub(crate) fn ram(&self) -> &[PageRange] {
        self.ram.as_slice()
    }

    /// 借用尚未移交给元数据的可用页并集。
    pub(crate) fn free(&self) -> &[PageRange] {
        self.free.as_slice()
    }

    /// 借用原始保留资源与整页 RAM 的交集。
    pub(crate) fn reserved(&self) -> &[PageRange] {
        self.reserved.as_slice()
    }

    /// 借用已实际分配给启动元数据的页并集。
    pub(crate) fn metadata(&self) -> &[PageRange] {
        self.metadata.as_slice()
    }

    /// 从真实区间计算互斥页角色的账务。
    pub(crate) fn stats(&self) -> BootStats {
        BootStats {
            present_pages: self.ram.pages(),
            reserved_pages: self.reserved.pages(),
            metadata_pages: self.metadata.pages(),
            free_pages: self.free.pages(),
        }
    }

    /// 从单个可用区间取得对齐连续页并登记元数据；错误不改变页拥有状态。
    pub(crate) fn allocate(
        &mut self,
        pages: usize,
        alignment_pages: usize,
    ) -> Result<PageRange, &'static str> {
        if pages == 0 || !alignment_pages.is_power_of_two() {
            return Err("invalid boot allocation size or alignment");
        }
        pages
            .checked_mul(PAGE_SIZE)
            .ok_or("boot allocation size overflows")?;
        alignment_pages
            .checked_mul(PAGE_SIZE)
            .ok_or("boot allocation alignment overflows")?;
        if self.allocation_count == MAX_METADATA_ALLOCATIONS {
            return Err("boot metadata allocation capacity exceeded");
        }
        for index in 0..self.free.count {
            let free = self.free.entries[index];
            let start = free
                .start
                .checked_add(alignment_pages - 1)
                .ok_or("boot allocation alignment overflows")?
                & !(alignment_pages - 1);
            let end = start
                .checked_add(pages)
                .ok_or("boot allocation end overflows")?;
            if free.end < end {
                continue;
            }
            let prefix = free.start < start;
            let suffix = end < free.end;
            if prefix && suffix && self.free.count == RANGE_CAPACITY {
                return Err("boot free range capacity exceeded");
            }
            let allocated = PageRange { start, end };
            self.metadata.insert(allocated)?;
            match (prefix, suffix) {
                (true, true) => {
                    self.free
                        .entries
                        .copy_within(index + 1..self.free.count, index + 2);
                    self.free.entries[index] = PageRange {
                        start: free.start,
                        end: start,
                    };
                    self.free.entries[index + 1] = PageRange {
                        start: end,
                        end: free.end,
                    };
                    self.free.count += 1;
                }
                (true, false) => self.free.entries[index].end = start,
                (false, true) => self.free.entries[index].start = end,
                (false, false) => {
                    self.free
                        .entries
                        .copy_within(index + 1..self.free.count, index);
                    self.free.count -= 1;
                }
            }
            self.allocation_count += 1;
            return Ok(allocated);
        }
        Err("no contiguous RAM for boot metadata")
    }
}

/// 用生产区间与分配算法检查对齐、空洞、保留交集和错误后的账务。
#[cfg(feature = "memory-tests")]
pub(crate) fn test_input_boundaries() -> bool {
    let mut info = BootInfo::empty([0; 3], 0);
    info.memory_count = 5;
    info.memory[..5].copy_from_slice(&[
        Region {
            start: 20 * PAGE_SIZE,
            end: 24 * PAGE_SIZE,
        },
        Region {
            start: PAGE_SIZE + 1,
            end: 5 * PAGE_SIZE,
        },
        Region {
            start: 4 * PAGE_SIZE,
            end: 8 * PAGE_SIZE,
        },
        Region {
            start: 8 * PAGE_SIZE,
            end: 12 * PAGE_SIZE - 1,
        },
        Region {
            start: 40 * PAGE_SIZE + 1,
            end: 40 * PAGE_SIZE + 2,
        },
    ]);
    info.reserved_count = 5;
    info.reserved[..5].copy_from_slice(&[
        Region {
            start: 3 * PAGE_SIZE + 1,
            end: 4 * PAGE_SIZE + 1,
        },
        Region {
            start: 4 * PAGE_SIZE,
            end: 5 * PAGE_SIZE,
        },
        Region {
            start: 5 * PAGE_SIZE,
            end: 6 * PAGE_SIZE,
        },
        Region {
            start: 10 * PAGE_SIZE + 1,
            end: 21 * PAGE_SIZE + 1,
        },
        Region {
            start: 30 * PAGE_SIZE,
            end: 31 * PAGE_SIZE,
        },
    ]);
    let Ok(mut memory) = BootMemory::new(&info) else {
        return false;
    };
    if memory.ram()
        != [
            PageRange { start: 2, end: 11 },
            PageRange { start: 20, end: 24 },
        ]
        || memory.reserved()
            != [
                PageRange { start: 3, end: 6 },
                PageRange { start: 10, end: 11 },
                PageRange { start: 20, end: 22 },
            ]
        || memory.free()
            != [
                PageRange { start: 2, end: 3 },
                PageRange { start: 6, end: 10 },
                PageRange { start: 22, end: 24 },
            ]
        || memory.stats()
            != (BootStats {
                present_pages: 13,
                reserved_pages: 6,
                metadata_pages: 0,
                free_pages: 7,
            })
    {
        return false;
    }
    if memory.allocate(2, 4) != Ok(PageRange { start: 8, end: 10 })
        || memory.allocate(2, 2) != Ok(PageRange { start: 6, end: 8 })
        || memory.metadata() != [PageRange { start: 6, end: 10 }]
        || memory.free()
            != [
                PageRange { start: 2, end: 3 },
                PageRange { start: 22, end: 24 },
            ]
        || memory.stats()
            != (BootStats {
                present_pages: 13,
                reserved_pages: 6,
                metadata_pages: 4,
                free_pages: 3,
            })
    {
        return false;
    }
    let before = memory.stats();
    if memory.allocate(3, 1).is_ok()
        || memory.allocate(0, 1).is_ok()
        || memory.allocate(1, 0).is_ok()
        || memory.allocate(1, 3).is_ok()
        || memory.allocate(usize::MAX, 1).is_ok()
        || memory.allocate(1, 1usize << (usize::BITS - 1)).is_ok()
        || memory.stats() != before
        || memory.metadata() != [PageRange { start: 6, end: 10 }]
    {
        return false;
    }
    info.memory_count = 2;
    info.reserved_count = 0;
    info.memory[0] = Region {
        start: PAGE_SIZE,
        end: PAGE_SIZE + PAGE_SIZE / 2,
    };
    info.memory[1] = Region {
        start: PAGE_SIZE + PAGE_SIZE / 2,
        end: 8 * PAGE_SIZE,
    };
    let Ok(mut split) = BootMemory::new(&info) else {
        return false;
    };
    if split.ram() != [PageRange { start: 1, end: 8 }]
        || split.allocate(2, 4) != Ok(PageRange { start: 4, end: 6 })
        || split.free()
            != [
                PageRange { start: 1, end: 4 },
                PageRange { start: 6, end: 8 },
            ]
        || split.metadata() != [PageRange { start: 4, end: 6 }]
        || split.stats()
            != (BootStats {
                present_pages: 7,
                reserved_pages: 0,
                metadata_pages: 2,
                free_pages: 5,
            })
    {
        return false;
    }
    info.memory_count = 1;
    info.reserved_count = 0;
    info.memory[0] = Region {
        start: usize::MAX - 1,
        end: usize::MAX,
    };
    if BootMemory::new(&info).is_ok() {
        return false;
    }
    info.memory[0] = Region {
        start: 0,
        end: 32 * PAGE_SIZE,
    };
    info.reserved_count = 1;
    info.reserved[0] = Region {
        start: usize::MAX - 1,
        end: usize::MAX,
    };
    if BootMemory::new(&info).is_ok() {
        return false;
    }
    info.reserved_count = 0;
    let Ok(mut capacity) = BootMemory::new(&info) else {
        return false;
    };
    for pfn in 0..MAX_METADATA_ALLOCATIONS {
        if capacity.allocate(1, 1)
            != Ok(PageRange {
                start: pfn,
                end: pfn + 1,
            })
        {
            return false;
        }
    }
    let before = capacity.stats();
    if capacity.allocate(1, 1).is_ok()
        || capacity.stats() != before
        || capacity.metadata()
            != [PageRange {
                start: 0,
                end: MAX_METADATA_ALLOCATIONS,
            }]
    {
        return false;
    }
    info.memory_count = MAX_MEMORY_REGIONS + 1;
    if BootMemory::new(&info).is_ok() {
        return false;
    }
    info.memory_count = 1;
    info.reserved_count = MAX_RESERVED_REGIONS + 1;
    if BootMemory::new(&info).is_ok() {
        return false;
    }
    info.reserved_count = 0;
    info.memory[0] = Region { start: 10, end: 10 };
    BootMemory::new(&info).is_err()
}
