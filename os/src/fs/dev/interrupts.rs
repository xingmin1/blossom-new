use crate::fs::{dirent::Dirent, DiskInodeType};
use alloc::sync::Arc;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;
use spin::Mutex;
use lazy_static::lazy_static;

use crate::{
    fs::{directory_tree::DirectoryTreeNode, file_trait::File, layout::Stat, StatMode},
    mm::UserBuffer,
    syscall::errno::{EACCES, ENOTDIR, ESPIPE},
};

/// 中断统计信息虚拟文件
/// 显示系统中各种中断的处理次数
pub struct Interrupts {
    /// 文件偏移量
    pub offset: Mutex<usize>,
}

lazy_static! {
    /// 全局中断计数器
    static ref INTERRUPT_COUNTS: Mutex<BTreeMap<usize, usize>> = Mutex::new(BTreeMap::new());
}

impl Interrupts {
    /// 创建新的 Interrupts 实例
    pub fn new() -> Self {
        Self {
            offset: Mutex::new(0),
        }
    }

    /// 增加指定中断号的处理次数
    pub fn increment_interrupt_count(irq: usize) {
        let mut counts = INTERRUPT_COUNTS.lock();
        *counts.entry(irq).or_insert(0) += 1;
        // 添加调试输出
        // println!("[interrupts] Incremented IRQ {} to {}", irq, counts[&irq]);
    }

    /// 获取中断统计信息的字符串表示
    fn get_interrupt_stats() -> String {
        let counts = INTERRUPT_COUNTS.lock();
        let mut result = String::new();
        
        // 确保按中断号严格递增顺序输出，并且每次读取都保持一致的顺序
        // 为了适配测试用例，我们需要确保IRQ号不会"减少"
        let mut sorted_irqs: Vec<usize> = counts.keys().cloned().collect();
        sorted_irqs.sort(); // 确保严格递增顺序
        
        for irq in sorted_irqs {
            if let Some(count) = counts.get(&irq) {
                result.push_str(&format!("{}:        {}\n", irq, count));
            }
        }
        
        result
    }

    /// 测试函数：手动增加一些中断计数用于测试
    #[cfg(test)]
    pub fn test_increment() {
        Self::increment_interrupt_count(5);  // 时钟中断
        Self::increment_interrupt_count(9);  // 外部中断
        Self::increment_interrupt_count(10); // 外部中断
    }

    /// 获取当前中断计数（用于调试）
    pub fn get_current_counts() -> BTreeMap<usize, usize> {
        INTERRUPT_COUNTS.lock().clone()
    }

    /// 调试函数：手动增加一些测试数据
    pub fn debug_add_test_data() {
        Self::increment_interrupt_count(5);   // 时钟中断
        Self::increment_interrupt_count(5);   // 时钟中断
        Self::increment_interrupt_count(5);   // 时钟中断
        Self::increment_interrupt_count(9);   // 外部中断
        Self::increment_interrupt_count(10);  // 外部中断
        println!("[interrupts] Added test data");
    }
}

impl File for Interrupts {
    fn deep_clone(&self) -> Arc<dyn File> {
        Arc::new(Interrupts {
            offset: Mutex::new(*self.offset.lock()),
        })
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        false // 不允许写入
    }

    fn read(&self, offset: Option<&mut usize>, buf: &mut [u8]) -> usize {
        unreachable!()
    }

    fn write(&self, offset: Option<&mut usize>, buf: &[u8]) -> usize {
        unreachable!()
    }

    fn r_ready(&self) -> bool {
        true
    }

    fn w_ready(&self) -> bool {
        false
    }

    fn get_size(&self) -> usize {
        Self::get_interrupt_stats().len()
    }

    fn get_stat(&self) -> Stat {
        Stat::new(
            crate::makedev!(0, 5),
            1,
            StatMode::S_IFREG.bits() | 0o444, // 只读文件
            1,
            crate::makedev!(1, 3),
            0,
            0,
            0,
            0,
        )
    }

    fn read_user(&self, offset: Option<usize>, mut buf: UserBuffer) -> usize {
        let stats = Self::get_interrupt_stats();
        let stats_bytes = stats.as_bytes();
        
        let start_offset = offset.unwrap_or_else(|| {
            let mut offset = self.offset.lock();
            let current_offset = *offset;
            *offset += buf.len();
            current_offset
        });
        
        if start_offset >= stats_bytes.len() {
            return 0; // EOF
        }
        
        let end_offset = (start_offset + buf.len()).min(stats_bytes.len());
        let read_len = end_offset - start_offset;
        
        buf.write(&stats_bytes[start_offset..end_offset]);
        read_len
    }

    fn write_user(&self, _offset: Option<usize>, _buf: UserBuffer) -> usize {
        ESPIPE as usize // 不允许写入
    }

    fn get_file_type(&self) -> DiskInodeType {
        DiskInodeType::File
    }

    fn info_dirtree_node(
        &self,
        _dirnode_ptr: alloc::sync::Weak<crate::fs::directory_tree::DirectoryTreeNode>,
    ) {
    }

    fn get_dirtree_node(&self) -> Option<Arc<DirectoryTreeNode>> {
        None
    }

    fn open(&self, _flags: crate::fs::layout::OpenFlags, _special_use: bool) -> Arc<dyn File> {
        Arc::new(Interrupts::new())
    }

    fn open_subfile(&self) -> Result<Vec<(String, Arc<dyn File>)>, isize> {
        Err(ENOTDIR)
    }

    fn create(&self, _name: &str, _file_type: DiskInodeType) -> Result<Arc<dyn File>, isize> {
        Err(EACCES)
    }

    fn link_child(&self, _name: &str, _child: &Self) -> Result<(), isize> {
        Err(EACCES)
    }

    fn unlink(&self, _delete: bool) -> Result<(), isize> {
        Err(EACCES)
    }

    fn get_dirent(&self, _count: usize) -> Vec<Dirent> {
        Vec::new()
    }

    fn lseek(&self, offset: isize, whence: crate::fs::layout::SeekWhence) -> Result<usize, isize> {
        let mut current_offset = self.offset.lock();
        let new_offset = match whence {
            crate::fs::layout::SeekWhence::SEEK_SET => offset,
            crate::fs::layout::SeekWhence::SEEK_CUR => *current_offset as isize + offset,
            crate::fs::layout::SeekWhence::SEEK_END => self.get_size() as isize + offset,
            _ => return Err(crate::syscall::errno::EINVAL),
        };
        
        if new_offset < 0 {
            return Err(crate::syscall::errno::EINVAL);
        }
        
        *current_offset = new_offset as usize;
        Ok(new_offset as usize)
    }

    fn modify_size(&self, _diff: isize) -> Result<(), isize> {
        Err(EACCES)
    }

    fn truncate_size(&self, _new_size: usize) -> Result<(), isize> {
        Err(EACCES)
    }

    fn set_timestamp(&self, _ctime: Option<usize>, _atime: Option<usize>, _mtime: Option<usize>) {
    }

    fn get_single_cache(&self, _offset: usize) -> Result<Arc<Mutex<crate::fs::cache::PageCache>>, ()> {
        Err(())
    }

    fn get_all_caches(&self) -> Result<Vec<Arc<Mutex<crate::fs::cache::PageCache>>>, ()> {
        Err(())
    }

    fn hang_up(&self) -> bool {
        false
    }

    fn fcntl(&self, _cmd: u32, _arg: u32) -> isize {
        -1
    }

    fn oom(&self) -> usize {
        0
    }
} 