//! 打开对象和 TCP 协议拥有分离；本连接的睡眠互斥不限制其它核心。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::fs::{FileStat, FsError, InodeKind, InodeTime};
use crate::net::tcp::TcpEndpoint;
use crate::net::NetStackRef;

pub const TCP_BUFFER_LEN: usize = 16 * 1024;
const O_RDWR: u32 = 2;
const O_APPEND: u32 = 0o2000;
const O_NONBLOCK: u32 = 0o4000;
const O_CLOEXEC: u32 = 0o2000000;

/// 打开对象持稳定协议端点、网络域和创建者凭据，并保存共享 OFD 状态位。
/// 构造只准备打开实例，不发表用户 fd。
pub struct SocketFile {
    endpoint: Arc<TcpEndpoint>,
    net: NetStackRef,
    uid: u32,
    gid: u32,
    flags: AtomicU32,
    opened: bool,
}

impl SocketFile {
    /// 从同一网络域准备默认有界 TCP 核心与新的打开实例，CLOEXEC 留给描述符槽。
    pub fn try_new_tcp(
        net: NetStackRef,
        flags: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Arc<Self>, FsError> {
        if flags & !(O_NONBLOCK | O_CLOEXEC) != 0 {
            return Err(FsError::InvalidOperation);
        }
        let endpoint = net.try_new_tcp(TCP_BUFFER_LEN, TCP_BUFFER_LEN)?;
        Self::allocate(net, endpoint, flags, uid, gid)
    }

    /// 为已拥有的本域协议端点构造独立 OFD，不搬走 raw 核心或替换稳定 listener。
    pub fn from_endpoint(
        net: NetStackRef,
        endpoint: Arc<TcpEndpoint>,
        flags: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Arc<Self>, FsError> {
        if flags & !(O_NONBLOCK | O_CLOEXEC) != 0 {
            return Err(FsError::InvalidOperation);
        }
        if !endpoint.belongs_to(&net) {
            return Err(FsError::InvalidOperation);
        }
        Self::allocate(net, endpoint, flags, uid, gid)
    }

    /// 分配失败撤回未交给打开实例的协议拥有，成功后才赋予末 OFD 关闭责任。
    fn allocate(
        net: NetStackRef,
        endpoint: Arc<TcpEndpoint>,
        flags: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Arc<Self>, FsError> {
        let result = Arc::try_new(Self {
            endpoint: endpoint.clone(),
            net,
            uid,
            gid,
            flags: AtomicU32::new(O_RDWR | (flags & O_NONBLOCK)),
            opened: false,
        });
        match result {
            Ok(mut file) => {
                Arc::get_mut(&mut file)
                    .expect("unpublished socket file is shared")
                    .opened = true;
                Ok(file)
            }
            Err(_) => {
                endpoint.abandon();
                Err(FsError::NoMemory)
            }
        }
    }

    /// 借用稳定端点拥有者，协议索引可 clone 此 Arc 而不保整张文件表。
    pub fn endpoint(&self) -> &Arc<TcpEndpoint> {
        &self.endpoint
    }

    /// 返回构造时的同一网络域；dup 和 fork 共享打开实例及这个引用。
    pub fn net(&self) -> &NetStackRef {
        &self.net
    }

    /// 访问模式固定为双向流，非阻塞状态属于真正的 OFD。
    pub fn flags(&self) -> u32 {
        self.flags.load(Ordering::Acquire)
    }

    /// 沿文件对象已有规则保存可变状态位，descriptor 的 CLOEXEC 不进入 OFD。
    pub fn set_status_flags(&self, raw: u32) -> Result<(), FsError> {
        self.flags
            .store(O_RDWR | (raw & (O_APPEND | O_NONBLOCK)), Ordering::Release);
        Ok(())
    }

    /// 匿名 socket 属性使用本域稳定身份和创建者凭据，不伪装 regular 或磁盘路径。
    pub fn stat(&self) -> FileStat {
        let zero = InodeTime {
            seconds: 0,
            nanoseconds: 0,
        };
        FileStat {
            inode: self.endpoint.id(),
            kind: InodeKind::Socket,
            mode: 0o140777,
            size: 0,
            uid: self.uid,
            gid: self.gid,
            links: 1,
            blocks: 0,
            device: 0,
            rdev: 0,
            block_size: self.net.loopback_mtu() as u32,
            atime: zero,
            mtime: zero,
            ctime: zero,
        }
    }
}

impl Drop for SocketFile {
    /// 末打开实例只请求协议关闭，不同步等待 peer、期限或后台 Work。
    fn drop(&mut self) {
        if self.opened {
            self.endpoint.close();
        }
    }
}
