//! TCP 原始入口只复制目标 ABI 字节，描述符与协议拥有沿同一文件表发表和归还。

use core::convert::TryFrom;

use crate::fs::{DescriptorFlags, FileIoContext, FileRef, ReadBuffer, WriteBuffer};
use crate::mm::space::Mm;
use crate::net::address::{InetEndpoint, SocketAddress, AF_INET, INET_ADDRESS_LEN};
use crate::net::socket::SocketFile;
use crate::syscall::error::{copyin, copyout, Errno, SysResult};
use crate::syscall::{Outcome, Request, Restart};
use crate::task::UserThread;

const SOCK_STREAM: u32 = 1;
const SOCK_TYPE_MASK: u32 = 0xf;
const SOCK_MAX: u32 = 11;
const SOCK_NONBLOCK: u32 = 0o4000;
const SOCK_CLOEXEC: u32 = 0o2000000;
const SOCKET_FLAGS: u32 = SOCK_NONBLOCK | SOCK_CLOEXEC;
const IPPROTO_TCP: i32 = 6;
const IPPROTO_MAX: i32 = 263;
const ADDRESS_STORAGE_LEN: usize = 128;
const SOL_SOCKET: i32 = 1;
const SO_TYPE: i32 = 3;
const SO_ERROR: i32 = 4;
const SO_SNDBUF: i32 = 7;
const SO_RCVBUF: i32 = 8;
const SO_ACCEPTCONN: i32 = 30;
const SO_PROTOCOL: i32 = 38;
const SO_DOMAIN: i32 = 39;

/// 无进度的协议中断交给原信号重启出口，其余结果仅在此处编码为负 errno。
pub(super) fn dispatch(user: &UserThread, request: Request) -> Outcome {
    let raw = request.arguments;
    let result = match request.number {
        198 => socket(user, raw),
        200 => bind(user, raw),
        201 => listen(user, raw),
        202 => accept(user, raw, 0),
        203 => connect(user, raw),
        204 => name(user, raw, false),
        205 => name(user, raw, true),
        206 => sendto(user, raw),
        207 => recvfrom(user, raw),
        208 => setsockopt(user, raw),
        209 => getsockopt(user, raw),
        210 => shutdown(user, raw),
        242 => accept(user, raw, raw[3] as u32),
        _ => unreachable!("non-TCP request entered network dispatcher"),
    };
    match result {
        Err(Errno::Interrupted) => Outcome::Restart(Restart::Syscall { request }),
        other => Outcome::Return(other.unwrap_or_else(Errno::result)),
    }
}

/// 按目标 int 截取描述符；负值不转成有效文件表索引。
fn descriptor(raw: usize) -> Result<usize, Errno> {
    let fd = raw as i32;
    if fd < 0 {
        Err(Errno::BadFd)
    } else {
        Ok(fd as usize)
    }
}

/// 文件表短锁只取得同一 OFD 的强引用，真实 socket 能力在锁外检查。
fn socket_file(user: &UserThread, raw: usize) -> Result<FileRef, Errno> {
    let runtime = user.runtime().expect("socket syscall has no runtime");
    let file = runtime.files.lookup(descriptor(raw)?)?;
    if file.socket().is_none() {
        return Err(Errno::NotSocket);
    }
    Ok(file)
}

/// 只将创建附加位传给 OFD，CLOEXEC 的真正状态由发表时的描述符槽保存。
fn socket(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let family = raw[0] as i32;
    let kind = raw[1] as u32;
    let protocol = raw[2] as i32;
    let flags = kind & !SOCK_TYPE_MASK;
    let kind = kind & SOCK_TYPE_MASK;
    if flags & !SOCKET_FLAGS != 0 {
        return Err(Errno::InvalidArgument);
    }
    if family != i32::from(AF_INET) {
        return Err(Errno::AddressFamilyNotSupported);
    }
    if SOCK_MAX <= kind {
        return Err(Errno::InvalidArgument);
    }
    if protocol < 0 || IPPROTO_MAX <= protocol {
        return Err(Errno::InvalidArgument);
    }
    if kind != SOCK_STREAM {
        return Err(Errno::SocketTypeNotSupported);
    }
    if protocol != 0 && protocol != IPPROTO_TCP {
        return Err(Errno::ProtocolNotSupported);
    }
    let runtime = user.runtime().expect("socket has no runtime");
    let reservation = runtime.files.reserve()?;
    let file: FileRef = SocketFile::try_new_tcp(
        runtime.net,
        flags,
        runtime.credentials.uid,
        runtime.credentials.gid,
    )?;
    Ok(reservation.publish(
        file,
        DescriptorFlags {
            close_on_exec: flags & SOCK_CLOEXEC != 0,
        },
    ) as isize)
}

/// 输入地址仅解释完整 sockaddr_in，family 和端口的字节序由共同地址模块处理。
fn input_address(mm: &Mm, address: usize, raw_length: usize) -> Result<InetEndpoint, Errno> {
    let length = raw_length as i32;
    if length < INET_ADDRESS_LEN as i32 || (ADDRESS_STORAGE_LEN as i32) < length {
        return Err(Errno::InvalidArgument);
    }
    let mut bytes = [0u8; INET_ADDRESS_LEN];
    copyin(mm, &mut bytes, address)?;
    match SocketAddress::decode(&bytes)? {
        SocketAddress::Inet(endpoint) => Ok(endpoint),
        _ => Err(Errno::AddressFamilyNotSupported),
    }
}

/// socklen 按目标四字节读取；负容量拒绝，零容量仍要求长度指针可读。
fn capacity(mm: &Mm, address: usize) -> Result<usize, Errno> {
    let mut bytes = [0u8; 4];
    copyin(mm, &mut bytes, address)?;
    let length = i32::from_ne_bytes(bytes);
    if length < 0 {
        Err(Errno::InvalidArgument)
    } else {
        Ok(length as usize)
    }
}

/// 只交付可容纳的名称前缀，再写完整名称长度；无名称接收只写真实长度零。
fn output_address(
    mm: &Mm,
    address: usize,
    length_address: usize,
    name: Option<SocketAddress>,
) -> Result<(), Errno> {
    let capacity = capacity(mm, length_address)?;
    let mut bytes = [0u8; INET_ADDRESS_LEN];
    let length = name.map_or(0, |name| name.encode(&mut bytes));
    let copied = capacity.min(length);
    if copied != 0 {
        copyout(mm, address, &bytes[..copied])?;
    }
    copyout(mm, length_address, &(length as u32).to_ne_bytes())
}

/// 绑定只消费复制后的内核端点，不跨协议操作借用用户地址。
fn bind(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let file = socket_file(user, raw[0])?;
    let mm = user.mm().expect("bind has no MM");
    let endpoint = input_address(&mm, raw[1], raw[2])?;
    file.socket()
        .expect("checked socket capability")
        .bind(endpoint)?;
    Ok(0)
}

/// signed backlog 按有限监听队列上界处理，零仍交给真实 listener。
fn listen(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let file = socket_file(user, raw[0])?;
    let backlog = (raw[1] as i32 as u32).min(128) as usize;
    file.socket()
        .expect("checked socket capability")
        .listen(backlog)?;
    Ok(0)
}

/// 领取 child 前预留原 fd，名称复制失败只结束本次未发表 OFD 和预留槽。
fn accept(user: &UserThread, raw: [usize; 6], flags: u32) -> SysResult {
    if flags & !SOCKET_FLAGS != 0 {
        return Err(Errno::InvalidArgument);
    }
    let file = socket_file(user, raw[0])?;
    let runtime = user.runtime().expect("accept has no runtime");
    let reservation = runtime.files.reserve()?;
    let child = file.socket().expect("checked socket capability").accept(
        flags,
        runtime.credentials.uid,
        runtime.credentials.gid,
        FileIoContext::User(user),
    )?;
    if raw[1] != 0 {
        let mm = user.mm().expect("accept has no MM");
        output_address(&mm, raw[1], raw[2], Some(child.peer_address()?))?;
    }
    let child: FileRef = child;
    Ok(reservation.publish(
        child,
        DescriptorFlags {
            close_on_exec: flags & SOCK_CLOEXEC != 0,
        },
    ) as isize)
}

/// 连接等待和已有握手的中断重启由真实 OFD 处理，入口不重建协议状态。
fn connect(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let file = socket_file(user, raw[0])?;
    let mm = user.mm().expect("connect has no MM");
    let endpoint = input_address(&mm, raw[1], raw[2])?;
    file.socket()
        .expect("checked socket capability")
        .connect(endpoint, FileIoContext::User(user))?;
    Ok(0)
}

/// 名称来自同一协议端点，未连接 peer 的错误不合成为空地址。
fn name(user: &UserThread, raw: [usize; 6], peer: bool) -> SysResult {
    let file = socket_file(user, raw[0])?;
    let socket = file.socket().expect("checked socket capability");
    let address = if peer {
        socket.peer_address()?
    } else {
        socket.local_address()?
    };
    let mm = user.mm().expect("socket name has no MM");
    output_address(&mm, raw[1], raw[2], Some(address))?;
    Ok(0)
}

/// TCP 忽略单次目的名称，但原始名称字节仍经合法 copyin；发送量只取协议接受量。
fn sendto(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let mm = user.mm().expect("sendto has no MM");
    let mut source = WriteBuffer::user(&mm, raw[1], raw[2])?;
    let file = socket_file(user, raw[0])?;
    if raw[4] != 0 {
        let length = raw[5] as i32;
        if length < 0 || (ADDRESS_STORAGE_LEN as i32) < length {
            return Err(Errno::InvalidArgument);
        }
        let mut bytes = [0u8; ADDRESS_STORAGE_LEN];
        copyin(&mm, &mut bytes[..length as usize], raw[4])?;
    }
    let count = file.socket().expect("checked socket capability").send(
        &mut source,
        raw[3] as u32,
        FileIoContext::User(user),
    )?;
    Ok(count as isize)
}

/// 接收通过真实 ReadBuffer 消费前缀，TCP 无 source 名称时仍交付长度零。
fn recvfrom(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let mm = user.mm().expect("recvfrom has no MM");
    let mut target = ReadBuffer::user(&mm, raw[1], raw[2])?;
    let file = socket_file(user, raw[0])?;
    let message = file.socket().expect("checked socket capability").receive(
        &mut target,
        raw[3] as u32,
        FileIoContext::User(user),
    )?;
    if raw[4] != 0 {
        output_address(&mm, raw[4], raw[5], message.source)?;
    }
    Ok(message.copied as isize)
}

/// 未实现的选项修改明确拒绝，既不访问其内容，也不伪造协议设置成功。
fn setsockopt(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let _file = socket_file(user, raw[0])?;
    if (raw[4] as i32) < 0 {
        return Err(Errno::InvalidArgument);
    }
    Err(Errno::ProtocolOptionNotSupported)
}

/// 查询真实类型、监听状态和缓冲容量；pending error 在交付用户字节前一次消费。
fn getsockopt(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let file = socket_file(user, raw[0])?;
    let socket = file.socket().expect("checked socket capability");
    let level = raw[1] as i32;
    let option = raw[2] as i32;
    if level != SOL_SOCKET {
        return Err(Errno::ProtocolOptionNotSupported);
    }
    let mm = user.mm().expect("getsockopt has no MM");
    let capacity = capacity(&mm, raw[4])?;
    let value = match option {
        SO_TYPE => SOCK_STREAM as i32,
        SO_ERROR => socket
            .take_error()
            .map_or(0, |error| Errno::from(error) as i32),
        SO_SNDBUF => i32::try_from(socket.send_capacity()).map_err(|_| Errno::Overflow)?,
        SO_RCVBUF => i32::try_from(socket.receive_capacity()).map_err(|_| Errno::Overflow)?,
        SO_ACCEPTCONN => i32::from(socket.is_listener()),
        SO_PROTOCOL => IPPROTO_TCP,
        SO_DOMAIN => i32::from(AF_INET),
        _ => return Err(Errno::ProtocolOptionNotSupported),
    };
    let bytes = value.to_ne_bytes();
    let copied = capacity.min(bytes.len());
    if copied != 0 {
        copyout(&mm, raw[3], &bytes[..copied])?;
    }
    copyout(&mm, raw[4], &(copied as u32).to_ne_bytes())?;
    Ok(0)
}

/// 收发方向关闭直接作用于原 TCP 端点，不用关闭描述符冒充 shutdown。
fn shutdown(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let file = socket_file(user, raw[0])?;
    file.socket()
        .expect("checked socket capability")
        .shutdown(raw[1] as u32)?;
    Ok(0)
}
