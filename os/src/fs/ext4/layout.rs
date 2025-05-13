#![allow(unused)]
use crate::{
    config::PAGE_SIZE,
    fs::{
        directory_tree::DirectoryTreeNode,
        dirent::Dirent,
        ext4::{
            block_group::Block,
            direntry::{DirEntryType, Ext4DirEntryTail},
            InodeFileType, PageCache,
        },
        file_trait::File,
        inode::{InodeLock, InodeTrait},
        vfs::VFS,
        DiskInodeType, OpenFlags, SeekWhence, Stat, StatMode,
    },
    lang_items::Bytes,
    mm::UserBuffer,
    syscall::errno::{EINVAL, ENOTDIR, ENOTEMPTY},
};
use alloc::{
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use spin::{Mutex, RwLock};

use core::{
    convert::TryInto,
    fmt::Debug,
    mem, panic,
    ptr::{addr_of, addr_of_mut, read},
};

use super::{
    direntry::Ext4DirEntry,
    ext4fs::Ext4FileSystem,
    file::{Ext4FileContent, Ext4FileContentWrapper},
    Cache, Ext4Inode, Ext4InodeRef, InodePerm, PageCacheManager,
};

// 可能后续会用到？
pub enum ExtType {
    Ext2,
    Ext3,
    Ext4,
}

// 对Ext4Inode的一层封装，用于构成与OSInode同级别的结构体
pub struct Ext4OSInode {
    /// 是否可读
    readable: bool,
    /// 是否可写
    writable: bool,
    /// 被进程使用的计数
    special_use: bool,
    /// 是否追加
    append: bool,
    /// 具体的Inode
    inode: Arc<Mutex<Ext4InodeRef>>,
    /// 文件偏移
    offset: Mutex<usize>,
    /// 目录树节点指针
    dirnode_ptr: Arc<Mutex<Weak<DirectoryTreeNode>>>,
    /// ext4fs实例
    ext4fs: Arc<Ext4FileSystem>,
    /// inode锁
    inode_lock: Arc<RwLock<InodeLock>>,
    /// 文件缓存
    file_cache_manager: Arc<PageCacheManager>,
}

impl Ext4OSInode {
    // 只在获取根目录时使用
    pub fn new(root_inode: Ext4InodeRef, ext4fs: Arc<Ext4FileSystem>) -> Arc<dyn File> {
        Arc::new(Self {
            inode_lock: Arc::new(RwLock::new(InodeLock {})),
            readable: true,
            writable: true,
            special_use: true,
            append: false,
            inode: Arc::new(Mutex::new(root_inode)),
            offset: Mutex::new(0),
            dirnode_ptr: Arc::new(Mutex::new(Weak::new())),
            ext4fs,
            file_cache_manager: Arc::new(PageCacheManager::new()),
        })
    }
}

impl Ext4OSInode {
    pub fn first_root_inode(ext4fs: &Arc<dyn VFS>) -> Arc<dyn File> {
        let ext4fs_concrete = Arc::downcast::<Ext4FileSystem>(ext4fs.clone()).unwrap();
        // 先获取ROOT_INODE

        let root_inode = todo!();
        let ext4_root_inode = Ext4OSInode::new(root_inode, ext4fs_concrete);
        todo!()
    }
}

impl Drop for Ext4OSInode {
    fn drop(&mut self) {
        if self.special_use {
            let inode = self.get_dirtree_node();
            match inode {
                Some(inode) => inode.sub_special_use(),
                None => {}
            }
        }
    }
}

#[allow(unused)]
impl File for Ext4OSInode {
    fn deep_clone(&self) -> Arc<dyn File> {
        if self.special_use {
            let inode = self.get_dirtree_node();
            match inode {
                Some(inode) => inode.add_special_use(),
                None => {}
            }
        }
        Arc::new(Self {
            // 这下面的这一行可能会有问题
            inode_lock: Arc::new(RwLock::new(InodeLock {})),
            readable: self.readable,
            writable: self.writable,
            special_use: self.special_use,
            append: self.append,
            inode: self.inode.clone(),
            offset: Mutex::new(*self.offset.lock()),
            dirnode_ptr: self.dirnode_ptr.clone(),
            ext4fs: self.ext4fs.clone(),
            file_cache_manager: self.file_cache_manager.clone(),
        })
    }

    fn readable(&self) -> bool {
        self.readable
    }

    fn writable(&self) -> bool {
        self.writable
    }

    /// 在偏移量为offset的位置读取信息
    fn read(&self, offset: Option<&mut usize>, buffer: &mut [u8]) -> usize {
        let inode_ref = self.inode.lock();
        match offset {
            Some(offset) => {
                let mut start = *offset;
                let size = inode_ref.inode.size() as usize;
                // 比较大小，看要读取多大
                let end = (offset.clone() + buffer.len()).min(size);
                if start >= end {
                    return 0;
                }
                let mut start_cache = start / PageCacheManager::CACHE_SZ;
                let mut read_size = 0;
                loop {
                    // 计算当前块的结束位置
                    let mut end_current_block =
                        (start / PageCacheManager::CACHE_SZ + 1) * PageCacheManager::CACHE_SZ;
                    end_current_block = end_current_block.min(end);
                    // 读取并更新读取长度
                    // TODO: 后期记得尝试加锁！
                    let block_read_size = end_current_block - start;
                    self.file_cache_manager
                        .get_cache(
                            start_cache,
                            || -> Vec<usize> {
                                self.get_neighboring_blk(start_cache, Arc::new(inode_ref.clone()))
                            },
                            &self.ext4fs.block_device,
                        )
                        .lock()
                        .read(0, |data_block: &[u8; 4096]| {
                            let dst = &mut buffer[read_size..read_size + block_read_size];
                            let src = &data_block[start % PageCacheManager::CACHE_SZ
                                ..start % PageCacheManager::CACHE_SZ + block_read_size];
                            dst.copy_from_slice(src);
                        });
                    read_size += block_read_size;

                    if end_current_block == end {
                        break;
                    }
                    start_cache += 1;
                    start = end_current_block;
                }
                *offset = read_size;
                read_size
            }
            None => {
                let mut offset = self.offset.lock();
                let mut start = *offset;
                let size = inode_ref.inode.size() as usize;
                // 比较大小，看要读取多大
                let end = (offset.clone() + buffer.len()).min(size);
                if start >= end {
                    return 0;
                }
                let mut start_cache = start / PageCacheManager::CACHE_SZ;
                let mut read_size = 0;
                loop {
                    // 计算当前块的结束位置
                    let mut end_current_block =
                        (start / PageCacheManager::CACHE_SZ + 1) * PageCacheManager::CACHE_SZ;
                    end_current_block = end_current_block.min(end);
                    // 读取并更新读取长度
                    // TODO: 后期记得尝试加锁！
                    let block_read_size = end_current_block - start;
                    self.file_cache_manager
                        .get_cache(
                            start_cache,
                            || -> Vec<usize> {
                                self.get_neighboring_blk(start_cache, Arc::new(inode_ref.clone()))
                            },
                            &self.ext4fs.block_device,
                        )
                        .lock()
                        .read(0, |data_block: &[u8; 4096]| {
                            let dst = &mut buffer[read_size..read_size + block_read_size];
                            let src = &data_block[start % PageCacheManager::CACHE_SZ
                                ..start % PageCacheManager::CACHE_SZ + block_read_size];
                            dst.copy_from_slice(src);
                        });
                    read_size += block_read_size;

                    if end_current_block == end {
                        break;
                    }
                    start_cache += 1;
                    start = end_current_block;
                }
                *offset += read_size;
                read_size
            }
        }
    }

    fn write(&self, offset: Option<&mut usize>, buf: &[u8]) -> usize {
        // println!("into here!!!");
        // println!("buf is :{:?}", buf);
        let mut total_write_size = 0usize;

        // 获取 inode 的文件大小
        let old_size;
        {
            let inode_ref = self.inode.lock();
            old_size = inode_ref.inode.get_file_size() as usize;
        }

        // 获取写锁
        let inode_lock = self.inode_lock.write();

        match offset {
            Some(offset) => {
                let mut start = *offset;
                let diff_len = buf.len() as isize + start as isize - old_size as isize;
        
                if diff_len > 0 {
                    self.truncate_size((old_size as isize + diff_len) as usize).unwrap();
                }
        
                let inode_ref = self.inode.lock();
                total_write_size = self.update_block_cache(start, buf, Arc::new(inode_ref.clone()));
                *offset += total_write_size;
            }
            None => {
                let mut offset = self.offset.lock();
                let start = *offset;
                let diff_len = buf.len() as isize + start as isize - old_size as isize;
        
                if diff_len > 0 {
                    self.truncate_size((old_size as isize + diff_len) as usize).unwrap();
                }
        
                let inode_ref = self.inode.lock();
                total_write_size = self.update_block_cache(start, buf, Arc::new(inode_ref.clone()));
                *offset += total_write_size;
            }
        }
        total_write_size
    }

    fn r_ready(&self) -> bool {
        true
    }

    fn w_ready(&self) -> bool {
        true
    }

    fn read_user(&self, offset: Option<usize>, mut buf: UserBuffer) -> usize {
        let mut total_read_size = 0usize;

        let inode_lock = self.inode_lock.read();
        let inode_ref = self.inode.lock();
        match offset {
            Some(mut offset) => {
                let mut offset = &mut offset;
                for slice in buf.buffers.iter_mut() {
                    let read_size =
                        self.read_at_block_cache(*offset, *slice, Arc::new(inode_ref.clone()));
                    if read_size == 0 {
                        break;
                    }
                    *offset += read_size;
                    total_read_size += read_size;
                }
            }
            None => {
                let mut offset = self.offset.lock();
                for slice in buf.buffers.iter_mut() {
                    let read_size =
                        self.read_at_block_cache(*offset, *slice, Arc::new(inode_ref.clone()));
                    if read_size == 0 {
                        break;
                    }
                    *offset += read_size;
                    total_read_size += read_size;
                }
            }
        }
        total_read_size
    }

    fn write_user(&self, offset: Option<usize>, buf: UserBuffer) -> usize {
        let mut total_write_size = 0usize;
        let inode_ref = self.inode.lock();
        let inode_num = inode_ref.inode_num;
        let inode_lock = self.inode_lock.write();
        match offset {
            Some(mut offset) => {
                let mut offset = &mut offset;
                for slice in buf.buffers.iter() {
                    // TODO: 假定ext4_rs的处理是正确的
                    let write_size = self.ext4fs.write_at(inode_num, *offset, slice);
                    // 对块设备对象进行写入之后，更新缓存对象。
                    self.update_block_cache(offset.clone(), slice, Arc::new(inode_ref.clone()));
                    if let Ok(write_size) = write_size {
                        if write_size == 0 {
                            break;
                        }
                        *offset += write_size;
                        total_write_size += write_size;
                    }
                }
            }
            None => {
                let mut offset = self.offset.lock();
                for slice in buf.buffers.iter() {
                    let write_size = self.ext4fs.write_at(inode_num, *offset, slice);
                    self.update_block_cache(offset.clone(), slice, Arc::new(inode_ref.clone()));
                    if let Ok(write_size) = write_size {
                        if write_size == 0 {
                            break;
                        }
                        *offset += write_size;
                        total_write_size += write_size;
                    }
                }
            }
        }
        total_write_size
    }

    /// 获取文件大小
    /// 需要修改
    fn get_size(&self) -> usize {
        let inode_ref = self.inode.lock();
        inode_ref.inode.get_file_size() as usize
    }

    /// 获取文件状态
    fn get_stat(&self) -> crate::fs::Stat {
        let inode_ref = self.inode.lock();
        let size = inode_ref.inode.get_file_size() as usize;
        let atime = inode_ref.inode.atime();
        let mtime = inode_ref.inode.mtime();
        let ctime = inode_ref.inode.ctime();

        let st_mod: u32 = {
            if inode_ref.inode.get_file_type() == DiskInodeType::Directory {
                (StatMode::S_IFDIR | StatMode::S_IRWXU | StatMode::S_IRWXG | StatMode::S_IRWXO)
                    .bits()
            } else {
                (StatMode::S_IFREG | StatMode::S_IRWXU | StatMode::S_IRWXG | StatMode::S_IRWXO)
                    .bits()
            }
        };
        Stat::new(
            // 下面的时间用i64有点逆天了
            // 后面可能得把Stat改一下
            crate::makedev!(8, 0),
            inode_ref.inode_num as u64,
            st_mod,
            1,
            0,
            size as i64,
            atime as i64,
            mtime as i64,
            ctime as i64,
        )
    }

    /// 获取文件类型
    fn get_file_type(&self) -> DiskInodeType {
        // 利用inode的file_type字段
        self.inode.lock().inode.get_file_type()
    }

    fn info_dirtree_node(&self, dirnode_ptr: Weak<DirectoryTreeNode>) {
        *self.dirnode_ptr.lock() = dirnode_ptr;
    }

    /// 获取目录树节点
    fn get_dirtree_node(&self) -> Option<Arc<DirectoryTreeNode>> {
        self.dirnode_ptr.lock().upgrade()
    }

    /// 打开文件
    fn open(&self, flags: OpenFlags, special_use: bool) -> Arc<dyn File> {
        Arc::new(Self {
            readable: flags.contains(OpenFlags::O_RDONLY) || flags.contains(OpenFlags::O_RDWR),
            writable: flags.contains(OpenFlags::O_WRONLY) || flags.contains(OpenFlags::O_RDWR),
            special_use,
            append: flags.contains(OpenFlags::O_APPEND),
            inode: self.inode.clone(),
            offset: Mutex::new(0),
            dirnode_ptr: self.dirnode_ptr.clone(),
            ext4fs: self.ext4fs.clone(),
            inode_lock: self.inode_lock.clone(),
            file_cache_manager: self.file_cache_manager.clone(),
        })
    }

    /// 获取子文件列表
    fn open_subfile(&self) -> Result<Vec<(String, Arc<dyn File>)>, isize> {
        // 先获取inode
        // let inode_ref = self.inode.clone();
        let inode_ref = self.inode.lock();
        // 获取所有的子文件
        // TODO: Maybe Wrong
        // let entries = self.ext4fs.dir_get_entries_from_inode_ref(Arc::new(inode_ref.clone()));
        if inode_ref.inode.get_file_type() != DiskInodeType::Directory {
            return Err(ENOTDIR);
        }
        let entries = self.ext4fs.dir_get_entries(inode_ref.inode_num);
        // for entry in entries.iter() {
        //     println!("[kernel get subfile test] {:?}", entry.get_name());
        // }

        // 子文件构造闭包，用于upcast
        let get_dyn_file = |entry: &Ext4DirEntry| -> Arc<dyn File> {
            Arc::new(Self {
                inode_lock: Arc::new(RwLock::new(InodeLock {})),
                readable: true,
                writable: true,
                special_use: false,
                append: false,
                inode: Arc::new(Mutex::new(self.ext4fs.get_inode_ref(entry.inode))),
                offset: Mutex::new(0),
                dirnode_ptr: Arc::new(Mutex::new(Weak::new())),
                ext4fs: self.ext4fs.clone(),
                // maybe wrong
                file_cache_manager: Arc::new(PageCacheManager::new()),
            })
        };

        // let vec: Vec<(String, Arc<dyn File>)> = entries.iter().map(|entry| (entry.get_name(), get_dyn_file(entry))).collect();
        // Ok(vec)
        Ok(entries
            .into_iter()
            .map(|entry| (entry.get_name(), get_dyn_file(&entry)))
            .collect())
    }

    /// 创建文件
    /// # 参数
    /// name: 文件名
    /// file_type: 文件类型
    /// # 返回值
    /// + 文件对象
    fn create(
        &self,
        name: &str,
        file_type: crate::fs::DiskInodeType,
    ) -> Result<Arc<dyn File>, isize> {
        let inode_lock = self.inode_lock.write();
        // 获取inode_mode
        let inode_mode = match file_type {
            DiskInodeType::File => InodeFileType::S_IFREG.bits(),
            DiskInodeType::Directory => InodeFileType::S_IFDIR.bits(),
            _ => todo!(),
        };

        let inode_ref = self.inode.lock();
        let mut parent_inode_num = inode_ref.inode_num.clone();
        //println!("self(parent) inode num:{:?} self(parent) name:{:?}", parent_inode_num, inode_ref);
        let mut nameoff = 0;
        //println!("in here!!!???");
        if inode_mode == InodeFileType::S_IFDIR.bits() {
            let new_inode_num = self.ext4fs.generic_open(
                name,
                &mut parent_inode_num,
                true,
                inode_mode,
                &mut nameoff,
            );
            //println!("inhere???");
            if let Ok(new_inode_num) = new_inode_num {
                let new_inode_ref = self.ext4fs.get_inode_ref(new_inode_num);
                //println!("new_inode_ref:{:#?}", new_inode_ref);
                return Ok(Arc::new(Self {
                    inode_lock: Arc::new(RwLock::new(InodeLock {})),
                    readable: true,
                    writable: true,
                    special_use: false,
                    append: false,
                    inode: Arc::new(Mutex::new(new_inode_ref)),
                    offset: Mutex::new(0),
                    dirnode_ptr: Arc::new(Mutex::new(Weak::new())),
                    ext4fs: self.ext4fs.clone(),
                    // maybe wrong
                    file_cache_manager: Arc::new(PageCacheManager::new()),
                }));
            } else {
                panic!()
            }
        }
        //println!("[kernel] name={} inode_mode={}", name, inode_mode);
        let inode_perm = (InodePerm::S_IREAD | InodePerm::S_IWRITE).bits();
        //println!("[kernel] inode_perm = {}", inode_perm);
        let new_inode_ref = self
            .ext4fs
            // .create(self.inode.inode_num, name, inode_mode | inode_perm);
            .create(parent_inode_num, name, inode_mode | inode_perm);
        // xein TODO: 此处有问题
        //println!("cre1");
        if let Ok(inode_ref) = new_inode_ref {
            //println!("Successfully here");
            return Ok(Arc::new(Self {
                inode_lock: Arc::new(RwLock::new(InodeLock {})),
                readable: true,
                writable: true,
                special_use: false,
                append: false,
                inode: Arc::new(Mutex::new(inode_ref)),
                offset: Mutex::new(0),
                dirnode_ptr: Arc::new(Mutex::new(Weak::new())),
                ext4fs: self.ext4fs.clone(),
                // maybe wrong
                file_cache_manager: Arc::new(PageCacheManager::new()),
            }))
        } else {
            panic!()
        }
    }

    fn link_child(&self, name: &str, child: &Self) -> Result<(), isize>
    where
        Self: Sized,
    {
        todo!()
    }

    // remove file
    fn unlink(&self, delete: bool) -> Result<(), isize> {
        let inode_num = self.inode.lock().inode_num;
        let file_type = self.inode.lock().inode.get_file_type();

        // 如果是非空目录，返回错误
        if file_type == DiskInodeType::Directory
            && self.ext4fs.dir_has_entry(inode_num)
        {
            return Err(ENOTEMPTY);
        }

        // 先获取当前目录树节点，避免持锁后再调用 parent()
        let current_node = self.dirnode_ptr.lock().upgrade().unwrap();

        // 获取父目录节点（避免死锁）
        let parent_node = current_node.parent(); 
        let parent_file = parent_node.file.clone();
        let parent = Arc::downcast::<Ext4OSInode>(parent_file).unwrap();
        let mut parent_inode_ref = parent.inode.lock();

        // 重新获取子 inode，避免持锁冲突
        let mut child_inode_ref = self.ext4fs.get_inode_ref(inode_num);

        // 文件名
        let name = current_node.name.clone();

        // 如果是目录
        if file_type == DiskInodeType::Directory {
            self.ext4fs.truncate_inode(&mut child_inode_ref, 0)?;
            //println!("[kernel unlink] unlink dir name: {:?}", name);
            self.ext4fs.unlink(&mut parent_inode_ref, &mut child_inode_ref, name.as_str());
            self.ext4fs.write_back_inode(&mut parent_inode_ref);
            return Ok(());
        }

        // 如果是常规文件
        //println!("[kernel unlink] unlink file name: {:?}", name);
        self.ext4fs.unlink(&mut parent_inode_ref, &mut child_inode_ref, name.as_str());
        self.ext4fs.write_back_inode(&mut parent_inode_ref);

        Ok(())
    }

    /// 获取目录项
    /// # 参数
    /// + count：要获取的目录项数量
    /// # 返回值
    /// + 获取到的目录项数组/向量
    fn get_dirent(&self, count: usize) -> Vec<Dirent> {
        const DT_UNKNOWN: u8 = 0;
        const DT_DIR: u8 = 4;
        const DT_REG: u8 = 8;
        let inode_ref = self.inode.lock();
        assert!(inode_ref.inode.get_file_type() == DiskInodeType::Directory);
        let mut offset = self.offset.lock();
        let inode_lock = self.inode_lock.write();
        let vec = self.ext4fs.dir_get_entries(inode_ref.inode_num);

        let old_offset = *offset;

        // fat32下分多次进入get_dirent
        // ext4下要如何处理？
        // 不能单纯使用dir_get_entries直接进行，因为这样会出错
        // 每次都有项
        if let Some(ext4entry) = vec.last() {
            *offset = ext4entry.entry_len as usize;
        }
        // println!("[kernel] current offset2:{:?}", *offset);

        if old_offset == *offset {
            // 返回一个空的Vec数组
            return Vec::new();
        }

        // 此处的offset需要处理
        let result = vec
            .iter()
            .map(|ext4entry| {
                let d_type = match DirEntryType::from_bits(ext4entry.get_de_type()) {
                    // TODO:
                    // maybe wrong
                    Some(d_type) => match d_type {
                        DirEntryType::EXT4_DE_DIR => DT_DIR,
                        DirEntryType::EXT4_DE_REG_FILE => DT_REG,
                        _ => DT_UNKNOWN,
                    },
                    None => {
                        panic!("unknown entry type")
                    }
                };
                Dirent::new(
                    ext4entry.inode as usize,
                    ext4entry.entry_len as isize,
                    d_type,
                    &ext4entry.get_name().as_str(),
                )
            })
            .collect();
        // println!("[kernel in get_dirent] current offset is {:?}", offset);
        result
    }

    fn lseek(&self, offset: isize, whence: crate::fs::SeekWhence) -> Result<usize, isize> {
        let inode_lock = self.inode_lock.write();
        let new_offset = match whence {
            SeekWhence::SEEK_SET => offset,
            SeekWhence::SEEK_CUR => *self.offset.lock() as isize + offset,
            SeekWhence::SEEK_END => self.inode.lock().inode.get_file_size() as isize + offset,
            // whence is duplicated
            _ => return Err(EINVAL),
        };
        let new_offset = match new_offset < 0 {
            true => return Err(EINVAL),
            false => new_offset as usize,
        };
        *self.offset.lock() = new_offset;
        Ok(new_offset)
    }

    fn modify_size(&self, diff: isize) -> Result<(), isize> {
        println!("Should not into here!");
        // let inode_lock = self.inode_lock.write();
        let inode_ref = self.inode.lock();
        debug_assert!(diff.saturating_add(inode_ref.inode.size() as isize) >= 0);

        let old_size = inode_ref.inode.size() as u32;
        let new_size = (old_size as isize + diff) as u32;

        // drop(inode_lock);

        if diff > 0 {
            todo!()
        } else {
            self.file_cache_manager.notify_new_size(new_size as usize);
        }

        Ok(())
    }

    fn truncate_size(&self, new_size: usize) -> Result<(), isize> {
        let mut inode_ref = self.inode.lock();
        let result = self.ext4fs.truncate_inode(&mut inode_ref, new_size as u64);
        if let Ok(result) = result {
            Ok(())
        } else {
            panic!("truncate_inode failed: {:?}", result)
        }
    }

    fn set_timestamp(&self, ctime: Option<usize>, atime: Option<usize>, mtime: Option<usize>) {
        unsafe {
            // 将 Arc 转换为裸指针
            let ptr = Arc::as_ptr(&self.inode) as *mut Ext4Inode;
            // 直接修改数据
            if let Some(ctime) = ctime {
                (*ptr).set_ctime(ctime as u32);
            }
            if let Some(atime) = atime {
                (*ptr).set_atime(atime as u32);
            }
            if let Some(mtime) = mtime {
                (*ptr).set_mtime(mtime as u32);
            }
        }
    }

    /// 获取单个缓存页
    fn get_single_cache(&self, offset: usize) -> Result<Arc<Mutex<PageCache>>, ()> {
        let inode_ref = self.inode.lock();
        // 传入的 offset 实际上是cache号，或者说是第几个块
        // TODO:
        // 写到此处的时候还没有搞透彻pagecache到底是
        // 冗余缓存了相邻两个块
        // 还是真的有用到，后面有时间要再回去看看
        // 确保偏移量4KB对齐
        if offset & 0xfff != 0 {
            panic!("Invalid cache offset");
            return Err(());
        }
        // 将偏移量按页大小对齐并转换为缓存页ID
        let inner_cache_id = offset >> 12;
        let result = self.file_cache_manager.get_cache(
            inner_cache_id,
            || -> Vec<usize> {
                self.get_neighboring_blk(inner_cache_id, Arc::new(inode_ref.clone()))
            },
            &self.ext4fs.block_device,
        );
        Ok(result)
    }

    /// 获取所有缓存页
    /// 通过调用get_single_cache实现
    fn get_all_caches(&self) -> Result<Vec<Arc<Mutex<PageCache>>>, ()> {
        let inode_lock = self.inode_lock.read();
        // 参照fat_inode的get_all_cache，这里需要获取文件的大小，
        // 然后获取文件对应的缓存块（页）数量，
        // 然后初始化一个缓存页列表，
        // 将所有的缓存块（页）加入到缓存页中
        // 然后返回这个缓存列表
        // 那么如何获取大小呢？
        // 通过Ext4Inode的size方法
        let inode_ref = self.inode.lock();
        let file_size = inode_ref.inode.size();
        let cache_num =
            (file_size as usize + PageCacheManager::CACHE_SZ - 1) / PageCacheManager::CACHE_SZ;
        // println!(
        //     "[kernel in get_all_caches] file size: {} cache_num: {}",
        //     file_size, cache_num
        // );
        let mut cache_list = Vec::<Arc<Mutex<PageCache>>>::with_capacity(cache_num);
        // 使用自身的get_single_cache方法
        for cache_id in 0..cache_num {
            cache_list.push(
                self.get_single_cache_new(cache_id << 12, Arc::new(inode_ref.clone()))
                    .unwrap(),
            )
            // cache_list.push(self.get_single_cache(cache_id).unwrap())
        }
        Ok(cache_list)
    }

    /// 这个先不考虑实现
    fn oom(&self) -> usize {
        todo!()
    }

    /// 这个也一样
    fn hang_up(&self) -> bool {
        todo!()
    }

    /// 这个也一样
    fn fcntl(&self, cmd: u32, arg: u32) -> isize {
        todo!()
    }
}

impl Ext4OSInode {
    pub fn get_neighboring_blk(
        &self,
        inner_cache_id: usize,
        inode_ref: Arc<Ext4InodeRef>,
    ) -> Vec<usize> {
        // fat32下是通过clus_list以及inner_cache_id
        // 所以这里也需要有一个类似的block_list以及inner_cache_id
        // 这里的block_list需要是data_block_list
        // 那么如何获取其数据块列表？

        // 计算缓存页内包含的逻辑块范围
        let byts_per_blk = self.ext4fs.block_size;
        // 每个缓存页面对应的物理块数
        let blk_per_cache = PageCacheManager::CACHE_SZ / byts_per_blk;
        // 计算数据块起始块号和结束块号
        // inner_cache_id 从0开始，到(文件大小 + 4KB - 1)/4KB
        let mut blk_id = inner_cache_id * blk_per_cache;

        // 初始化用于存储缓存页面需要加载的数据块号集合
        let mut block_ids = Vec::with_capacity(blk_per_cache);

        let file_size = inode_ref.inode.size() as usize;
        // 获取所占数据块数
        let blk_cnts = (file_size + byts_per_blk - 1) / byts_per_blk;
        for _ in 0..blk_per_cache {
            if blk_id >= blk_cnts {
                // println!(
                //     "[kernel in get_neighboring_blk] blk_id is out of bound, blk_id: {}, blk_cnts: {}",
                //     blk_id, blk_cnts
                // );
                break;
            }
            // 获取物理块号
            // TODO: 此处可能有问题
            let start_block_id = self
                .ext4fs
                .get_pblock_idx(&inode_ref, blk_id as u32)
                .unwrap();
            block_ids.push(start_block_id as usize);
            blk_id += 1;
        }
        // println!("[kernel in get_neighboring_blk] block_ids: {:?}", block_ids);
        block_ids
    }

    /// 从指定offset位置的cache读取内容
    pub fn read_at_block_cache(
        &self,
        offset: usize,
        buffer: &mut [u8],
        inode_ref: Arc<Ext4InodeRef>,
    ) -> usize {
        let mut start = offset;
        let size = inode_ref.inode.size() as usize;
        // 比较大小，看要读取多大
        let end = (offset.clone() + buffer.len()).min(size);
        if start >= end {
            return 0;
        }
        let mut start_cache = start / PageCacheManager::CACHE_SZ;
        let mut read_size = 0;
        loop {
            // 计算当前块的结束位置
            let mut end_current_block =
                (start / PageCacheManager::CACHE_SZ + 1) * PageCacheManager::CACHE_SZ;
            end_current_block = end_current_block.min(end);
            // 读取并更新读取长度
            // TODO: 后期记得尝试加锁！
            let block_read_size = end_current_block - start;
            self.file_cache_manager
                .get_cache(
                    start_cache,
                    || -> Vec<usize> { self.get_neighboring_blk(start_cache, inode_ref.clone()) },
                    &self.ext4fs.block_device,
                )
                .lock()
                .read(0, |data_block: &[u8; PAGE_SIZE]| {
                    let dst = &mut buffer[read_size..read_size + block_read_size];
                    let src = &data_block[start % PageCacheManager::CACHE_SZ
                        ..start % PageCacheManager::CACHE_SZ + block_read_size];
                    dst.copy_from_slice(src);
                });
            read_size += block_read_size;

            if end_current_block == end {
                break;
            }
            start_cache += 1;
            start = end_current_block;
        }
        read_size
    }
    /// 获取单个缓存页
    fn get_single_cache_new(
        &self,
        offset: usize,
        inode_ref: Arc<Ext4InodeRef>,
    ) -> Result<Arc<Mutex<PageCache>>, ()> {
        // 传入的 offset 实际上是cache号，或者说是第几个块
        // TODO:
        // 写到此处的时候还没有搞透彻pagecache到底是
        // 冗余缓存了相邻两个块
        // 还是真的有用到，后面有时间要再回去看看
        // 确保偏移量4KB对齐
        if offset & 0xfff != 0 {
            panic!("Invalid cache offset");
            return Err(());
        }
        // 将偏移量按页大小对齐并转换为缓存页ID
        let inner_cache_id = offset >> 12;
        let result = self.file_cache_manager.get_cache(
            inner_cache_id,
            || -> Vec<usize> {self.get_neighboring_blk(inner_cache_id, inode_ref.clone())},
            &self.ext4fs.block_device,
        );
        Ok(result)
    }
}

impl Ext4OSInode {
    fn update_block_cache(&self, offset: usize, buf: &[u8], inode_ref: Arc<Ext4InodeRef>) -> usize {
        let mut start = offset;
        let old_size = inode_ref.inode.get_file_size() as usize;
        let diff_len = buf.len() as isize + offset as isize - old_size as isize;
        let end = (offset + buf.len()).min(old_size as usize);

        debug_assert!(start <= end);

        let mut start_cache = start / PageCacheManager::CACHE_SZ;
        let mut write_size = 0;
        loop {
            // 计算当前块的结束位置
            let mut end_current_block =
                (start / PageCacheManager::CACHE_SZ + 1) * PageCacheManager::CACHE_SZ;
            end_current_block = end_current_block.min(end);
            // 写入并更新写入的大小
            let block_write_size = end_current_block - start;
            self.file_cache_manager
                .get_cache(
                    start_cache,
                    || -> Vec<usize> { self.get_neighboring_blk(start_cache, inode_ref.clone()) },
                    &self.ext4fs.block_device,
                )
                .lock()
                .modify(0, |data_block: &mut [u8; PAGE_SIZE]| {
                    let src = &buf[write_size..write_size + block_write_size];
                    let dst = &mut data_block[start % PageCacheManager::CACHE_SZ
                        ..start % PageCacheManager::CACHE_SZ + block_write_size];
                    dst.copy_from_slice(src);
                });
            write_size += block_write_size;
            // move to next block
            if end_current_block == end {
                break;
            }
            start_cache += 1;
            start = end_current_block;
        }
        write_size
    }
}
