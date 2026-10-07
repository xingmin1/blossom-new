use crate::fs::poll::{ppoll, pselect, FdSet, PollFd};
use crate::fs::*;
use crate::fs::dev::pipe::Pipe;
use crate::hal::BLOCK_SZ;
use crate::mm::{
    copy_from_user, copy_to_user, copy_to_user_array, translated_refmut, translated_str,
    try_get_from_user, MapPermission, VirtAddr,
};
use crate::task::{current_task, current_user_token};
use crate::timer::TimeSpec;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::panic;
use log::{info, trace, warn};
use downcast_rs::DowncastSync;

use super::errno::*;

pub const AT_FDCWD: usize = 100usize.wrapping_neg();

/// Splice data between file descriptors
/// 
/// The splice() system call moves data between two file descriptors without copying
/// between kernel address space and user address space. It transfers up to len bytes
/// of data from the file descriptor fd_in to the file descriptor fd_out, where one
/// of the file descriptors must refer to a pipe.
/// 
/// # Arguments
/// * `fd_in` - File descriptor to read from
/// * `off_in` - Pointer to offset for input file (NULL for current position)
/// * `fd_out` - File descriptor to write to  
/// * `off_out` - Pointer to offset for output file (NULL for current position)
/// * `len` - Number of bytes to transfer
/// * `flags` - Splice flags (currently ignored)
/// 
/// # Returns
/// * Number of bytes transferred on success
/// * Negative error code on failure
pub fn sys_splice(
    fd_in: usize,
    off_in: *mut usize,
    fd_out: usize,
    off_out: *mut usize,
    len: usize,
    _flags: u32,
) -> isize {
    let task = current_task().unwrap();
    let fd_table = task.files.lock();
    
    // Get input file descriptor
    let in_file = match fd_table.get_ref(fd_in) {
        Ok(file_descriptor) => file_descriptor,
        Err(errno) => return errno,
    };
    
    // Get output file descriptor
    let out_file = match fd_table.get_ref(fd_out) {
        Ok(file_descriptor) => file_descriptor,
        Err(errno) => return errno,
    };
    
    info!("[sys_splice] fd_in: {}, fd_out: {}, len: {}", fd_in, fd_out, len);
    
    // Check if files are readable/writable
    if !in_file.readable() {
        return EBADF;
    }
    if !out_file.writable() {
        return EBADF;
    }
    
    // Handle offset pointers
    let token = task.get_user_token();
    let mut off_in_ptr = if off_in.is_null() {
        None
    } else {
        match translated_refmut(token, off_in) {
            Ok(offset) => {
                if (*offset as isize) < 0 {
                    return EINVAL;
                }
                Some(offset)
            }
            Err(errno) => return errno,
        }
    };
    
    let mut off_out_ptr = if off_out.is_null() {
        None
    } else {
        match translated_refmut(token, off_out) {
            Ok(offset) => {
                if (*offset as isize) < 0 {
                    return EINVAL;
                }
                Some(offset)
            }
            Err(errno) => return errno,
        }
    };
    
    info!("[sys_splice] off_in: {:?}, off_out: {:?}", off_in_ptr, off_out_ptr);
    
    // Use a kernel buffer for data transfer
    const BUFFER_SIZE: usize = 4096;
    let mut buffer = Vec::<u8>::with_capacity(BUFFER_SIZE);
    let mut total_transferred = 0usize;
    let mut remaining = len;
    
    // Check if input is a pipe
    let is_input_pipe = in_file.file.downcast_ref::<Pipe>().is_some();
    let mut first_read = true;
    
    while remaining > 0 {
        // Determine how much to read
        let read_size = remaining.min(BUFFER_SIZE);
        unsafe {
            buffer.set_len(read_size);
        }
        
        // Read from input file
        let bytes_read = in_file.read(off_in_ptr.as_deref_mut(), buffer.as_mut_slice());
        if (bytes_read as isize) < 0 {
            let errno = bytes_read as isize;
            return errno;
        } else if bytes_read == 0 {
            // EOF reached
            break;
        }
        
        // Adjust buffer size to actual bytes read
        unsafe {
            buffer.set_len(bytes_read);
        }
        
        // Write to output file
        let bytes_written = out_file.write(off_out_ptr.as_deref_mut(), buffer.as_slice());
        if (bytes_written as isize) < 0 {
            let errno = bytes_written as isize;
            return errno;
        } else if bytes_written == 0 {
            // No more space to write
            break;
        }
        
        total_transferred += bytes_written;
        remaining -= bytes_read;
        
        // If we couldn't write all the data we read, we're done
        if bytes_written < bytes_read {
            break;
        }
        
        // For pipes, if this is the first read and we got less data than requested,
        // don't try to read more (avoids blocking on empty pipe with open write end)
        if is_input_pipe && first_read && bytes_read < read_size {
            break;
        }
        
        first_read = false;
    }
    
    info!("[sys_splice] transferred bytes: {}", total_transferred);
    total_transferred as isize
}

/// # Warning
/// `fs` & `files` is locked in this function
fn __openat(dirfd: usize, path: &str) -> Result<FileDescriptor, isize> {
    let task = current_task().unwrap();
    let file_descriptor = match dirfd {
        AT_FDCWD => task.fs.lock().working_inode.as_ref().clone(),
        fd => {
            let fd_table = task.files.lock();
            match fd_table.get_ref(fd) {
                Ok(file_descriptor) => file_descriptor.clone(),
                Err(errno) => return Err(errno),
            }
        }
    };
    file_descriptor.open(path, OpenFlags::O_RDONLY, false)
}

/// If offset is not NULL, then it points to a variable holding the
/// file offset from which sendfile() will start reading data from
/// in_fd.
///
/// When sendfile() returns,
/// this variable will be set to the offset of the byte following
/// the last byte that was read.
///
/// If offset is not NULL, then sendfile() does not modify the file
/// offset of in_fd; otherwise the file offset is adjusted to reflect
/// the number of bytes read from in_fd.
///
/// If offset is NULL, then data will be read from in_fd starting at
/// the file offset, and the file offset will be updated by the call.
pub fn sys_sendfile(out_fd: usize, in_fd: usize, offset: *mut usize, count: usize) -> isize {
    let task = current_task().unwrap();
    let fd_table = task.files.lock();
    let in_file = match fd_table.get_ref(in_fd) {
        Ok(file_descriptor) => file_descriptor,
        Err(errno) => return errno,
    };
    let out_file = match fd_table.get_ref(out_fd) {
        Ok(file_descriptor) => file_descriptor,
        Err(errno) => return errno,
    };
    info!("[sys_sendfile] outfd: {}, in_fd: {}", out_fd, in_fd);
    if !in_file.readable() || !out_file.writable() {
        return EBADF;
    }

    let token = task.get_user_token();
    // turn a pointer in user space into a pointer in kernel space if it is not null
    let offset = if offset.is_null() {
        offset
    } else {
        match translated_refmut(token, offset) {
            Ok(offset) => offset as *mut usize,
            Err(errno) => return errno,
        }
    };

    // a buffer in kernel
    const BUFFER_SIZE: usize = 4096;
    let mut buffer = Vec::<u8>::with_capacity(BUFFER_SIZE);
    let mut buffer_ptr: Option<&[u8]> = None;

    let mut left_bytes = count;
    loop {
        let write_buffer = match buffer_ptr {
            Some(buffer_ptr) => buffer_ptr,
            None => {
                unsafe {
                    buffer.set_len(left_bytes.min(BUFFER_SIZE));
                }
                let read_size = in_file.read(unsafe { offset.as_mut() }, buffer.as_mut_slice());
                if (read_size as isize) < 0 {
                    let errno = read_size as isize;
                    return errno;
                } else if read_size == 0 {
                    break;
                }
                unsafe {
                    buffer.set_len(read_size);
                }
                buffer.as_slice()
            }
        };

        let read_size = write_buffer.len();

        let fallback = |redundant_bytes: usize| unsafe {
            let offset = offset.as_mut();
            match offset {
                Some(offset) => {
                    *offset -= redundant_bytes;
                }
                None => match in_file.lseek(-(redundant_bytes as isize), SeekWhence::SEEK_CUR) {
                    Ok(_) => {}
                    Err(errno) => panic!("failed! errno {}", errno),
                },
            }
        };

        let write_size = out_file.write(None, write_buffer);
        if (write_size as isize) < 0 {
            fallback(read_size);
            let errno = read_size as isize;
            return errno;
        } else if write_size == 0 {
            fallback(read_size);
            break;
        }

        buffer_ptr = if write_size < read_size {
            Some(&write_buffer[write_size..])
        } else {
            None
        };
        left_bytes -= write_size;
    }
    let send_size = count - left_bytes;
    info!("[sys_sendfile] send bytes: {}", send_size);
    send_size as isize
}

/// Copy a range of data from one file to another
/// 
/// The copy_file_range() system call performs an in-kernel copy between two file descriptors
/// without the additional cost of transferring data from the kernel to user space and then
/// back into the kernel. It copies up to len bytes of data from the source file descriptor
/// fd_in to the target file descriptor fd_out, overwriting any data that exists within the
/// requested range of the target file.
/// 
/// # Arguments
/// * `fd_in` - File descriptor to read from
/// * `off_in` - Pointer to offset for input file (NULL for current position)
/// * `fd_out` - File descriptor to write to  
/// * `off_out` - Pointer to offset for output file (NULL for current position)
/// * `len` - Number of bytes to transfer
/// * `flags` - Copy flags (currently must be 0)
/// 
/// # Returns
/// * Number of bytes transferred on success
/// * Negative error code on failure
pub fn sys_copy_file_range(
    fd_in: usize,
    off_in: *mut isize,
    fd_out: usize,
    off_out: *mut isize,
    len: usize,
    flags: u32,
) -> isize {
    // 1. flags 必须为 0
    if flags != 0 {
        return EINVAL;
    }

    // 2. 取文件对象并检查可读写
    let task = current_task().unwrap();
    let fd_table = task.files.lock();
    let in_file = match fd_table.get_ref(fd_in) {
        Ok(file) => file,
        Err(e) => return e as isize,
    };
    let out_file = match fd_table.get_ref(fd_out) {
        Ok(file) => file,
        Err(e) => return e as isize,
    };
    if !in_file.readable() || !out_file.writable() {
        return EBADF;
    }

    // 3. 翻译用户传入的 off_in/off_out 指针，并校验负值
    let token = task.get_user_token();
    // off_in
    let mut off_in_user = if !off_in.is_null() {
        let user_ptr = match translated_refmut(token, off_in) {
            Ok(ptr) => ptr,
            Err(e) => return e as isize,
        };
        let cur = *user_ptr;
        if cur < 0 {
            return EINVAL;
        }
        Some((user_ptr, cur as usize))
    } else {
        None
    };
    // off_out
    let mut off_out_user = if !off_out.is_null() {
        let user_ptr = match translated_refmut(token, off_out) {
            Ok(ptr) => ptr,
            Err(e) => return e as isize,
        };
        let cur = *user_ptr;
        if cur < 0 {
            return EINVAL;
        }
        Some((user_ptr, cur as usize))
    } else {
        None
    };

    // 4. len = 0 时根据规范：将目标文件大小置为0，然后返回 0
    if len == 0 {
        // 将目标文件的大小置为 0
        match out_file.truncate_size(0) {
            Ok(_) => {}
            Err(e) => {
                return e;
            }
        }
        
        // 更新用户偏移（如果有的话）
        if let Some((user_ptr, _)) = off_in_user {
            *user_ptr = *user_ptr; // 保持原值
        }
        if let Some((user_ptr, _)) = off_out_user {
            *user_ptr = *user_ptr; // 保持原值
        }
        return 0;
    }

    // 5. 检查输入文件的偏移是否超过文件大小
    let in_file_size = in_file.get_size();
    let in_read_pos = if let Some((_, offset)) = off_in_user {
        offset
    } else {
        in_file.get_offset()
    };
    
    // 如果读取位置超过文件大小，直接返回 0
    if in_read_pos >= in_file_size {
        return 0;
    }

    // 6. 确保写入位置之前的字节都为 0
    // 获取输出文件的写入位置
    let out_write_pos = if let Some((_, offset)) = off_out_user {
        offset
    } else {
        out_file.get_offset()
    };
    
    // 如果写入位置不为 0，需要确保从文件开始到写入位置的所有字节都为 0
    if out_write_pos > 0 {
        // 获取当前文件大小
        let current_size = out_file.get_size();
        
        // 如果当前文件大小小于写入位置，需要扩展文件并填充 0
        if current_size < out_write_pos {
            let padding_needed = out_write_pos - current_size;
            
            // 创建一个全 0 的缓冲区来填充
            let mut zero_buf = Vec::new();
            zero_buf.resize(4096, 0u8);
            let mut remaining_padding = padding_needed;
            let mut padding_offset = current_size;
            
            while remaining_padding > 0 {
                let padding_size = remaining_padding.min(4096);
                let padding_buf = &zero_buf[..padding_size];
                
                let write_result: isize = out_file.write(Some(&mut padding_offset), padding_buf) as isize;
                
                remaining_padding -= padding_size;
            }
        }
    }

    // 7. 真正的拷贝循环
    const BUF_SZ: usize = 4096;
    let mut buf = Vec::with_capacity(BUF_SZ);
    let mut leftover = len;
    let mut copied = 0;



    while leftover > 0 {
        // 7.1 准备读缓冲
        let to_read = leftover.min(BUF_SZ);
        unsafe { buf.set_len(to_read) };
        
        let r: isize = if let Some((_, ref mut cur)) = off_in_user {
            in_file.read(Some(cur), &mut buf[..]) as isize
        } else {
            in_file.read(None, &mut buf[..]) as isize
        };
        
        if r == 0 {
            break; // EOF
        }
        unsafe { buf.set_len(r as usize) };

        // 7.2 写数据
        let w: isize = if let Some((_, ref mut cur)) = off_out_user {
            out_file.write(Some(cur), &buf) as isize
        } else {
            out_file.write(None, &buf) as isize
        };
        
        if w == 0 {
            break;
        }

        leftover -= w as usize;
        copied += w as usize;
    }

    // 8. 拷贝结束后，把新的偏移写回用户指针
    if let Some((user_ptr, new_off)) = off_in_user {
        *user_ptr = new_off as isize;
    }
    if let Some((user_ptr, new_off)) = off_out_user {
        *user_ptr = new_off as isize;
    }

    copied as isize
}

/// # Warning
/// Only O_CLOEXEC is supported now
pub fn sys_pipe2(pipefd: usize, flags: u32) -> isize {
    const VALID_FLAGS: OpenFlags = OpenFlags::from_bits_truncate(
        0o2000000 /* O_CLOEXEC */ | 0o40000 /* O_DIRECT */ | 0o4000, /* O_NONBLOCK */
    );
    let flags = match OpenFlags::from_bits(flags) {
        Some(flags) => {
            // only O_CLOEXEC | O_DIRECT | O_NONBLOCK are valid in pipe2()
            if flags.difference(VALID_FLAGS).is_empty() {
                flags
            } else {
                // some flags are invalid in pipe2(), they are all valid OpenFlags though
                warn!(
                    "[sys_pipe2] invalid flags: {:?}",
                    flags.difference(VALID_FLAGS)
                );
                return EINVAL;
            }
        }
        None => {
            // contains invalid OpenFlags
            warn!("[sys_pipe2] unknown flags");
            return EINVAL;
        }
    };
    let task = current_task().unwrap();
    let mut fd_table = task.files.lock();
    let (pipe_read, pipe_write) = make_pipe();
    let read_fd = match fd_table.insert(FileDescriptor::new(
        flags.contains(OpenFlags::O_CLOEXEC),
        false,
        pipe_read,
    )) {
        Ok(fd) => fd,
        Err(errno) => return errno,
    };
    let write_fd = match fd_table.insert(FileDescriptor::new(
        flags.contains(OpenFlags::O_CLOEXEC),
        false,
        pipe_write,
    )) {
        Ok(fd) => fd,
        Err(errno) => return errno,
    };

    let token = task.get_user_token();
    if copy_to_user_array(
        token,
        [read_fd as u32, write_fd as u32].as_ptr(),
        pipefd as *mut u32,
        2,
    )
    .is_err()
    {
        log::error!("[sys_pipe2] Failed to copy to {:?}", pipefd);
        return EFAULT;
    };
    info!(
        "[sys_pipe2] read_fd: {}, write_fd: {}, flags: {:?}",
        read_fd, write_fd, flags
    );
    SUCCESS
}

bitflags! {
    pub struct FstatatFlags: u32 {
        const AT_EMPTY_PATH = 0x1000;
        const AT_NO_AUTOMOUNT = 0x800;
        const AT_SYMLINK_NOFOLLOW = 0x100;
    }
}

/// warning: 此函数没有完全实现，没有实现根据mask来填充statx的值，并且没有直接维护statx结构体，通过stat结构体间接实现
pub fn sys_statx(dirfd: usize, path: *const u8, flags: u32, mask: u32, buf: *mut u8) -> isize {
    let token = current_user_token();
    let path = match translated_str(token, path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    let flags = match FstatatFlags::from_bits(flags) {
        Some(flags) => flags,
        None => {
            warn!("[sys_statx] unknown flags");
            return EINVAL;
        }
    };

    info!(
        "[sys_statx] dirfd: {}, path: {:?}, flags: {:?}",
        dirfd as isize, path, flags,
    );

    let task = current_task().unwrap();
    let file_descriptor = match dirfd {
        AT_FDCWD => task.fs.lock().working_inode.as_ref().clone(),
        fd => {
            let fd_table = task.files.lock();
            match fd_table.get_ref(fd) {
                Ok(file_descriptor) => file_descriptor.clone(),
                Err(errno) => return errno,
            }
        }
    };

    match file_descriptor.open(&path, OpenFlags::O_RDONLY, false) {
        Ok(file_descriptor) => {
            if copy_to_user(token, &file_descriptor.get_statx(mask), buf as *mut Statx).is_err() {
                log::error!("[sys_statx] Failed to copy to {:?}", buf);
                return EFAULT;
            };
            log::debug!("[sys_statx] statx:\n{:?}", file_descriptor.get_statx(mask));
            SUCCESS
        }
        Err(errno) => errno,
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Statfs {
    /// Type of filesystem
    f_type: usize,
    /// Optimal transfer block size
    f_bsize: usize,
    /// Total data blocks in filesystem
    f_blocks: u64,
    /// Free blocks in filesystem
    f_bfree: u64,
    /// Free blocks available to
    /// unprivileged user
    f_bavail: u64,
    /// Total file nodes in filesystem
    f_files: u64,
    /// Free file nodes in filesystem
    f_ffree: u64,
    /// Filesystem ID
    f_fsid: [i32; 2],
    /// Maximum length of filenames
    f_namelen: usize,
    /// Fragment size (since Linux 2.6)
    f_frsize: usize,
    /// Mount flags of filesystem
    f_flag: usize,
    /// Padding bytes reserved for future use
    f_spare: [usize; 4],
}
/// Fake implement for statfs syscall
pub fn sys_statfs(_path: *const u8, buf: *mut Statfs) -> isize {
    let statfs = Box::new(Statfs {
        f_type: 0xf2f52010,
        f_bsize: BLOCK_SZ,
        f_blocks: 10000,
        f_bfree: 9000,
        f_bavail: 9000,
        f_files: 1000,
        f_ffree: 960,
        f_fsid: [114, 514],
        f_namelen: 256,
        f_frsize: 0,
        f_flag: 0,
        f_spare: [0; 4],
    });
    let token = current_task().unwrap().get_user_token();
    if copy_to_user(token, statfs.as_ref(), buf).is_err() {
        log::error!("[sys_statfs] Failed to copy to {:?}", buf);
        return EFAULT;
    };
    SUCCESS
}

pub fn sys_fsync(fd: usize) -> isize {
    let task = current_task().unwrap();

    info!("[sys_fsync] fd: {}", fd);
    let fd_table = task.files.lock();
    if let Err(errno) = fd_table.check(fd) {
        return errno;
    }
    SUCCESS
}

pub fn sys_fchmodat() -> isize {
    // baseline 未完成这个函数
    println!("[kernel in sys_fchmodat] chmod is not supported for now!\n");
    0
}

pub fn sys_renameat2(
    olddirfd: usize,
    oldpath: *const u8,
    newdirfd: usize,
    newpath: *const u8,
    flags: u32,
) -> isize {
    let task = current_task().unwrap();
    let token = task.get_user_token();
    let oldpath = match translated_str(token, oldpath) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    let newpath = match translated_str(token, newpath) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    info!(
        "[sys_renameat2] olddirfd: {}, oldpath: {}, newdirfd: {}, newpath: {}, flags: {}",
        olddirfd as isize, oldpath, newdirfd as isize, newpath, flags
    );

    let old_file_descriptor = match olddirfd {
        AT_FDCWD => task.fs.lock().working_inode.as_ref().clone(),
        fd => {
            let fd_table = task.files.lock();
            match fd_table.get_ref(fd) {
                Ok(file_descriptor) => file_descriptor.clone(),
                Err(errno) => return errno,
            }
        }
    };
    let new_file_descriptor = match newdirfd {
        AT_FDCWD => task.fs.lock().working_inode.as_ref().clone(),
        fd => {
            let fd_table = task.files.lock();
            match fd_table.get_ref(fd) {
                Ok(file_descriptor) => file_descriptor.clone(),
                Err(errno) => return errno,
            }
        }
    };

    match FileDescriptor::rename(
        &old_file_descriptor,
        &oldpath,
        &new_file_descriptor,
        &newpath,
    ) {
        Ok(_) => SUCCESS,
        Err(errno) => errno,
    }
}

pub fn sys_ioctl(fd: usize, cmd: u32, arg: usize) -> isize {
    let task = current_task().unwrap();
    let fd_table = task.files.lock();
    let file_descriptor = match fd_table.get_ref(fd) {
        Ok(file_descriptor) => file_descriptor,
        Err(errno) => return errno,
    };
    file_descriptor.ioctl(cmd, arg)
}

pub fn sys_ppoll(fds: usize, nfds: usize, tmo_p: usize, sigmask: usize) -> isize {
    ppoll(
        fds as *mut PollFd,
        nfds,
        tmo_p as *const TimeSpec,
        sigmask as *const crate::task::Signals,
    )
}

pub fn sys_mkdirat(dirfd: usize, path: *const u8, mode: u32) -> isize {
    let task = current_task().unwrap();
    let token = task.get_user_token();
    let path = match translated_str(token, path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    info!(
        "[sys_mkdirat] dirfd: {}, path: {}, mode: {:?}",
        dirfd as isize,
        path,
        StatMode::from_bits(mode)
    );
    let file_descriptor = match dirfd {
        AT_FDCWD => task.fs.lock().working_inode.as_ref().clone(),
        fd => {
            let fd_table = task.files.lock();
            match fd_table.get_ref(fd) {
                Ok(file_descriptor) => file_descriptor.clone(),
                Err(errno) => return errno,
            }
        }
    };
    match file_descriptor.mkdir(&path) {
        Ok(_) => SUCCESS,
        Err(errno) => errno,
    }
}

bitflags! {
    pub struct UnlinkatFlags: u32 {
        const AT_REMOVEDIR = 0x200;
    }
}

/// # Warning
/// Currently we have no hard-link so this syscall will remove file directly.
pub fn sys_unlinkat(dirfd: usize, path: *const u8, flags: u32) -> isize {
    let task = current_task().unwrap();
    let token = task.get_user_token();
    let path = match translated_str(token, path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    let flags = match UnlinkatFlags::from_bits(flags) {
        Some(flags) => flags,
        None => {
            warn!("[sys_unlinkat] unknown flags");
            return EINVAL;
        }
    };
    info!(
        "[sys_unlinkat] dirfd: {}, path: {}, flags: {:?}",
        dirfd as isize, path, flags
    );

    let file_descriptor = match dirfd {
        AT_FDCWD => task.fs.lock().working_inode.as_ref().clone(),
        fd => {
            let fd_table = task.files.lock();
            match fd_table.get_ref(fd) {
                Ok(file_descriptor) => file_descriptor.clone(),
                Err(errno) => return errno,
            }
        }
    };
    match file_descriptor.delete(&path, flags.contains(UnlinkatFlags::AT_REMOVEDIR)) {
        Ok(_) => SUCCESS,
        Err(errno) => errno,
    }
}

bitflags! {
    pub struct UmountFlags: u32 {
        const MNT_FORCE           =   1;
        const MNT_DETACH          =   2;
        const MNT_EXPIRE          =   4;
        const UMOUNT_NOFOLLOW     =   8;
    }
}

pub fn sys_umount2(target: *const u8, flags: u32) -> isize {
    if target.is_null() {
        return EINVAL;
    }
    let token = current_user_token();
    let target = match translated_str(token, target) {
        Ok(target) => target,
        Err(errno) => return errno,
    };
    let flags = match UmountFlags::from_bits(flags) {
        Some(flags) => flags,
        None => return EINVAL,
    };
    info!("[sys_umount2] target: {}, flags: {:?}", target, flags);
    warn!("[sys_umount2] fake implementation!");
    SUCCESS
}

bitflags! {
    pub struct MountFlags: usize {
        const MS_RDONLY         =   1;
        const MS_NOSUID         =   2;
        const MS_NODEV          =   4;
        const MS_NOEXEC         =   8;
        const MS_SYNCHRONOUS    =   16;
        const MS_REMOUNT        =   32;
        const MS_MANDLOCK       =   64;
        const MS_DIRSYNC        =   128;
        const MS_NOATIME        =   1024;
        const MS_NODIRATIME     =   2048;
        const MS_BIND           =   4096;
        const MS_MOVE           =   8192;
        const MS_REC            =   16384;
        const MS_SILENT         =   32768;
        const MS_POSIXACL       =   (1<<16);
        const MS_UNBINDABLE     =   (1<<17);
        const MS_PRIVATE        =   (1<<18);
        const MS_SLAVE          =   (1<<19);
        const MS_SHARED         =   (1<<20);
        const MS_RELATIME       =   (1<<21);
        const MS_KERNMOUNT      =   (1<<22);
        const MS_I_VERSION      =   (1<<23);
        const MS_STRICTATIME    =   (1<<24);
        const MS_LAZYTIME       =   (1<<25);
        const MS_NOREMOTELOCK   =   (1<<27);
        const MS_NOSEC          =   (1<<28);
        const MS_BORN           =   (1<<29);
        const MS_ACTIVE         =   (1<<30);
        const MS_NOUSER         =   (1<<31);
    }
}

pub fn sys_mount(
    source: *const u8,
    target: *const u8,
    filesystemtype: *const u8,
    mountflags: usize,
    data: *const u8,
) -> isize {
    if source.is_null() || target.is_null() || filesystemtype.is_null() {
        return EINVAL;
    }
    let token = current_user_token();
    let source = match translated_str(token, source) {
        Ok(source) => source,
        Err(errno) => return errno,
    };
    let target = match translated_str(token, target) {
        Ok(target) => target,
        Err(errno) => return errno,
    };
    let filesystemtype = match translated_str(token, filesystemtype) {
        Ok(filesystemtype) => filesystemtype,
        Err(errno) => return errno,
    };
    // infallible
    let mountflags = MountFlags::from_bits(mountflags).unwrap();
    info!(
        "[sys_mount] source: {}, target: {}, filesystemtype: {}, mountflags: {:?}, data: {:?}",
        source, target, filesystemtype, mountflags, data
    );
    warn!("[sys_mount] fake implementation!");
    SUCCESS
}

bitflags! {
    pub struct UtimensatFlags: u32 {
        const AT_SYMLINK_NOFOLLOW = 0x100;
    }
}

pub fn sys_utimensat(
    dirfd: usize,
    pathname: *const u8,
    times: *const [TimeSpec; 2],
    flags: u32,
) -> isize {
    const UTIME_NOW: usize = 0x3fffffff;
    const UTIME_OMIT: usize = 0x3ffffffe;
    let token = current_user_token();
    let path = if !pathname.is_null() {
        match translated_str(token, pathname) {
            Ok(path) => path,
            Err(errno) => return errno,
        }
    } else {
        String::new()
    };
    let flags = match UtimensatFlags::from_bits(flags) {
        Some(flags) => flags,
        None => {
            warn!("[sys_utimensat] unknown flags");
            return EINVAL;
        }
    };

    info!(
        "[sys_utimensat] dirfd: {}, path: {}, times: {:?}, flags: {:?}",
        dirfd as isize, path, times, flags
    );

    let inode = match __openat(dirfd, &path) {
        Ok(inode) => inode,
        Err(errno) => return errno,
    };

    let now = TimeSpec::now();
    let timespec = &mut [now; 2];
    let mut atime = Some(now.tv_sec);
    let mut mtime = Some(now.tv_sec);
    if !times.is_null() {
        if copy_from_user(token, times, timespec).is_err() {
            log::error!("[sys_utimensat] Failed to copy from {:?}", times);
            return EFAULT;
        };
        match timespec[0].tv_nsec {
            UTIME_NOW => (),
            UTIME_OMIT => atime = None,
            _ => atime = Some(timespec[0].tv_sec),
        }
        match timespec[1].tv_nsec {
            UTIME_NOW => (),
            UTIME_OMIT => mtime = None,
            _ => mtime = Some(timespec[1].tv_sec),
        }
    }

    inode.set_timestamp(None, atime, mtime).unwrap();
    SUCCESS
}

pub fn sys_pselect(
    nfds: usize,
    read_fds: *mut FdSet,
    write_fds: *mut FdSet,
    exception_fds: *mut FdSet,
    timeout: *mut TimeSpec,
    sigmask: *const crate::task::signal::Signals,
) -> isize {
    if (nfds as isize) < 0 {
        return EINVAL;
    }
    let token = current_user_token();
    let mut kread_fds = match try_get_from_user(token, read_fds) {
        Ok(fds) => fds,
        Err(errno) => return errno,
    };
    let mut kwrite_fds = match try_get_from_user(token, write_fds) {
        Ok(fds) => fds,
        Err(errno) => return errno,
    };
    let mut kexception_fds = match try_get_from_user(token, exception_fds) {
        Ok(fds) => fds,
        Err(errno) => return errno,
    };
    let ktimeout = match try_get_from_user(token, timeout) {
        Ok(timeout) => timeout,
        Err(errno) => return errno,
    };
    let mut ret = pselect(
        nfds,
        &mut kread_fds,
        &mut kwrite_fds,
        &mut kexception_fds,
        &ktimeout,
        sigmask,
    );
    /*
    WARNING! The EFAULT errno is NOT mentioned in man for Linux.
    However, it is mentioned in BSD man, so we keep it anyway.
     */
    if let Some(kread_fds) = &kread_fds {
        trace!("[pselect] read_fds: {:?}", kread_fds);
        if copy_to_user(token, kread_fds, read_fds).is_err() {
            log::error!("[sys_pselect] Error copying to read_fds {:?}", read_fds);
            ret = EFAULT;
        };
    }
    if let Some(kwrite_fds) = &kwrite_fds {
        trace!("[pselect] write_fds: {:?}", kwrite_fds);
        if copy_to_user(token, kwrite_fds, write_fds).is_err() {
            log::error!("[sys_pselect] Error copying to write_fds {:?}", write_fds);
            ret = EFAULT;
        };
    }
    if let Some(kexception_fds) = &kexception_fds {
        trace!("[pselect] exception_fds: {:?}", kexception_fds);
        if copy_to_user(token, kexception_fds, exception_fds).is_err() {
            log::error!(
                "[sys_pselect] Error copying to exception_fds {:?}",
                exception_fds
            );
            ret = EFAULT;
        };
    }
    ret
}

/// umask() sets the calling process's file mode creation mask (umask) to
/// mask & 0777 (i.e., only the file permission bits of mask are used),
/// and returns the previous value of the mask.
/// # WARNING
/// In current implementation, umask is always 0. This syscall won't do anything.
pub fn sys_umask(mask: u32) -> isize {
    info!("[sys_umask] mask: {:o}", mask);
    warn!(
        "[sys_umask] In current implementation, umask is always 0. This syscall won't do anything."
    );
    0
}

bitflags! {
    pub struct FaccessatMode: u32 {
        const F_OK = 0;
        const R_OK = 4;
        const W_OK = 2;
        const X_OK = 1;
    }
    pub struct FaccessatFlags: u32 {
        const AT_SYMLINK_NOFOLLOW = 0x100;
        const AT_EACCESS = 0x200;
    }
}

pub fn sys_faccessat2(dirfd: usize, pathname: *const u8, mode: u32, flags: u32) -> isize {
    let token = current_user_token();
    let pathname = match translated_str(token, pathname) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    let mode = match FaccessatMode::from_bits(mode) {
        Some(mode) => mode,
        None => {
            warn!("[sys_faccessat2] unknown mode");
            return EINVAL;
        }
    };
    let flags = match FaccessatFlags::from_bits(flags) {
        Some(flags) => flags,
        None => {
            warn!("[sys_faccessat2] unknown flags");
            return EINVAL;
        }
    };

    info!(
        "[sys_faccessat2] dirfd: {}, pathname: {}, mode: {:?}, flags: {:?}",
        dirfd as isize, pathname, mode, flags
    );

    // Do not check user's authority, because user group is not implemented yet.
    // All existing files can be accessed.
    match __openat(dirfd, pathname.as_str()) {
        Ok(_) => SUCCESS,
        Err(errno) => errno,
    }
}

bitflags! {
    pub struct MsyncFlags: u32 {
        const MS_ASYNC      =   1;
        const MS_INVALIDATE =   2;
        const MS_SYNC       =   4;
    }
}

pub fn sys_msync(addr: usize, length: usize, flags: u32) -> isize {
    if !VirtAddr::from(addr).aligned() {
        return EINVAL;
    }
    let flags = match MsyncFlags::from_bits(flags) {
        Some(flags) => flags,
        None => return EINVAL,
    };
    let task = current_task().unwrap();
    if !task
        .vm
        .lock()
        .contains_valid_buffer(addr, length, MapPermission::empty())
    {
        return ENOMEM;
    }
    info!(
        "[sys_msync] addr: {:X}, length: {:X}, flags: {:?}",
        addr, flags, flags
    );
    SUCCESS
}

pub fn sys_ftruncate(fd: usize, length: isize) -> isize {
    let task = current_task().unwrap();
    let fd_table = task.files.lock();
    let file_descriptor = match fd_table.get_ref(fd) {
        Ok(file_descriptor) => file_descriptor,
        Err(errno) => return errno,
    };
    match file_descriptor.truncate_size(length) {
        Ok(()) => SUCCESS,
        Err(errno) => errno,
    }
}
