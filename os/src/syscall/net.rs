//! socket 原始入口只复制目标 ABI 字节，描述符与协议拥有沿同一文件表发表和归还。

use core::convert::{TryFrom, TryInto};

use crate::fs::{DescriptorFlags, FileIoContext, FileRef, ReadBuffer, UserIoVector, WriteBuffer};
use crate::mm::space::Mm;
use crate::net::address::{SocketAddress, AF_INET, AF_UNIX, INET_ADDRESS_LEN};
use crate::net::socket::SocketFile;
use crate::syscall::error::{copyin, copyout, Errno, SysResult};
use crate::syscall::file::{import_vectors, MAX_IOV_COUNT};
use crate::syscall::{Outcome, Request, Restart};
use crate::task::UserThread;

const SOCK_STREAM: u32 = 1;
const SOCK_DGRAM: u32 = 2;
const SOCK_TYPE_MASK: u32 = 0xf;
const SOCK_MAX: u32 = 11;
const SOCK_NONBLOCK: u32 = 0o4000;
const SOCK_CLOEXEC: u32 = 0o2000000;
const SOCKET_FLAGS: u32 = SOCK_NONBLOCK | SOCK_CLOEXEC;
const IPPROTO_TCP: i32 = 6;
const IPPROTO_UDP: i32 = 17;
const TCP_NODELAY: i32 = 1;
const MSG_TRUNC: u32 = 0x20;
const IPPROTO_MAX: i32 = 263;
const ADDRESS_STORAGE_LEN: usize = 128;
const SOL_SOCKET: i32 = 1;
const SO_REUSEADDR: i32 = 2;
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
        199 => socketpair(user, raw),
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
        211 => sendmsg(user, raw),
        212 => recvmsg(user, raw),
        242 => accept(user, raw, raw[3] as u32),
        _ => Err(Errno::NoSyscall),
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
    match kind {
        SOCK_STREAM if protocol == 0 || protocol == IPPROTO_TCP => {}
        SOCK_DGRAM if protocol == 0 || protocol == IPPROTO_UDP => {}
        SOCK_STREAM | SOCK_DGRAM => return Err(Errno::ProtocolNotSupported),
        _ => return Err(Errno::SocketTypeNotSupported),
    }
    let runtime = user.runtime().expect("socket has no runtime");
    let reservation = runtime.files.reserve()?;
    let file: FileRef = if kind == SOCK_STREAM {
        SocketFile::try_new_tcp(
            runtime.net,
            flags,
            runtime.credentials.uid,
            runtime.credentials.gid,
        )?
    } else {
        SocketFile::try_new_udp(
            runtime.net,
            flags,
            runtime.credentials.uid,
            runtime.credentials.gid,
        )?
    };
    Ok(reservation.publish(
        file,
        DescriptorFlags {
            close_on_exec: flags & SOCK_CLOEXEC != 0,
        },
    ) as isize)
}

/// 两个描述符先预留，完整 pair 和 fd 数组交付成功后才分别发表 OFD。
fn socketpair(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let family = raw[0] as i32;
    let kind = raw[1] as u32;
    let protocol = raw[2] as i32;
    let flags = kind & !SOCK_TYPE_MASK;
    let kind = kind & SOCK_TYPE_MASK;
    if flags & !SOCKET_FLAGS != 0 || SOCK_MAX <= kind {
        return Err(Errno::InvalidArgument);
    }
    if protocol < 0 || IPPROTO_MAX <= protocol {
        return Err(Errno::InvalidArgument);
    }
    if family == i32::from(AF_INET) {
        match kind {
            SOCK_STREAM if protocol == 0 || protocol == IPPROTO_TCP => {}
            SOCK_DGRAM if protocol == 0 || protocol == IPPROTO_UDP => {}
            SOCK_STREAM | SOCK_DGRAM => return Err(Errno::ProtocolNotSupported),
            _ => return Err(Errno::SocketTypeNotSupported),
        }
        return Err(Errno::Unsupported);
    }
    if family != i32::from(AF_UNIX) {
        return Err(Errno::AddressFamilyNotSupported);
    }
    if kind != SOCK_STREAM {
        return Err(Errno::SocketTypeNotSupported);
    }
    if protocol != 0 {
        return Err(Errno::ProtocolNotSupported);
    }
    let runtime = user.runtime().expect("socketpair has no runtime");
    let first_slot = runtime.files.reserve()?;
    let second_slot = runtime.files.reserve()?;
    let [first, second] = SocketFile::try_new_unix_pair(
        runtime.net,
        flags,
        runtime.credentials.uid,
        runtime.credentials.gid,
    )?;
    let mut bytes = [0u8; 8];
    bytes[..4].copy_from_slice(&(first_slot.fd() as u32).to_ne_bytes());
    bytes[4..].copy_from_slice(&(second_slot.fd() as u32).to_ne_bytes());
    let mm = user.mm().expect("socketpair has no MM");
    copyout(&mm, raw[3], &bytes)?;
    let descriptor_flags = DescriptorFlags {
        close_on_exec: flags & SOCK_CLOEXEC != 0,
    };
    first_slot.publish(first, descriptor_flags);
    second_slot.publish(second, descriptor_flags);
    Ok(0)
}

/// 地址先复制为内核值，最短 family 可表示 UDP 解除 peer，IPv4 仍需完整地址。
fn input_address(mm: &Mm, address: usize, raw_length: usize) -> Result<SocketAddress, Errno> {
    let length = raw_length as i32;
    if length < 2 || (ADDRESS_STORAGE_LEN as i32) < length {
        return Err(Errno::InvalidArgument);
    }
    let mut bytes = [0u8; ADDRESS_STORAGE_LEN];
    copyin(mm, &mut bytes[..length as usize], address)?;
    Ok(SocketAddress::decode(&bytes[..length as usize])?)
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
    let length = output_name(mm, address, capacity, name)?;
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

/// 单次目的属于本次发送；TCP 保留忽略名称的语义，UDP 不修改默认 peer。
fn sendto(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let mm = user.mm().expect("sendto has no MM");
    let mut source = WriteBuffer::user(&mm, raw[1], raw[2])?;
    let file = socket_file(user, raw[0])?;
    let socket = file.socket().expect("checked socket capability");
    let destination = if raw[4] == 0 {
        None
    } else {
        let length = raw[5] as i32;
        if length < 0 || (ADDRESS_STORAGE_LEN as i32) < length {
            return Err(Errno::InvalidArgument);
        }
        send_address(&mm, socket, raw[4], length as usize)?
    };
    let count = socket.send(
        &mut source,
        destination,
        raw[3] as u32,
        FileIoContext::User(user),
    )?;
    Ok(count as isize)
}

/// 名称字节只复制一次；TCP 不解释名称，匿名 UNIX 将实际名称请求交给协议拒绝。
fn send_address(
    mm: &Mm,
    socket: &SocketFile,
    address: usize,
    length: usize,
) -> Result<Option<SocketAddress>, Errno> {
    let mut bytes = [0u8; ADDRESS_STORAGE_LEN];
    copyin(mm, &mut bytes[..length], address)?;
    if socket.protocol() == IPPROTO_TCP {
        Ok(None)
    } else if socket.domain() == AF_UNIX {
        Ok(Some(SocketAddress::UnixUnnamed))
    } else {
        Ok(Some(SocketAddress::decode(&bytes[..length])?))
    }
}

/// LP64 消息头只保留本次操作需要的内核值，不借用户对象或复制第二份向量。
struct MessageHeader {
    name: usize,
    name_capacity: usize,
    vectors: usize,
    vector_count: usize,
}

/// 一次取得目标 56 字节头，逐字段解释固定偏移，padding 和输入 flags 不参与操作。
fn message_header(mm: &Mm, address: usize) -> Result<MessageHeader, Errno> {
    let mut bytes = [0u8; 56];
    copyin(mm, &mut bytes, address)?;
    let name = message_word(&bytes, 0);
    let name_length = i32::from_ne_bytes(bytes[8..12].try_into().expect("fixed namelen field"));
    let vectors = message_word(&bytes, 16);
    let vector_count = message_word(&bytes, 24);
    let _control = message_word(&bytes, 32);
    let control_length = message_word(&bytes, 40);
    let _flags = u32::from_ne_bytes(bytes[48..52].try_into().expect("fixed flags field"));
    if MAX_IOV_COUNT < vector_count {
        return Err(Errno::MessageTooLarge);
    }
    let name_capacity = if name == 0 {
        0
    } else {
        if name_length < 0 {
            return Err(Errno::InvalidArgument);
        }
        (name_length as usize).min(ADDRESS_STORAGE_LEN)
    };
    if control_length != 0 {
        return Err(Errno::Unsupported);
    }
    Ok(MessageHeader {
        name,
        name_capacity,
        vectors,
        vector_count,
    })
}

/// 固定头内的 unsigned long 使用目标 LP64 宽度，不能由宿主结构布局决定。
fn message_word(bytes: &[u8; 56], offset: usize) -> usize {
    u64::from_ne_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("fixed LP64 field"),
    ) as usize
}

/// 一次 import 的原向量直接交同一 OFD，单次名称不改变 UDP 的默认 peer。
fn sendmsg(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let file = socket_file(user, raw[0])?;
    let socket = file.socket().expect("checked socket capability");
    let mm = user.mm().expect("sendmsg has no MM");
    let header = message_header(&mm, raw[1])?;
    let destination = if header.name_capacity == 0 {
        None
    } else {
        send_address(&mm, socket, header.name, header.name_capacity)?
    };
    let mut fast = [UserIoVector {
        address: 0,
        length: 0,
    }; 8];
    let vectors = import_vectors(&mm, header.vectors, header.vector_count, &mut fast)?;
    let mut source = WriteBuffer::user_vector(&mm, &vectors)?;
    Ok(socket.send(
        &mut source,
        destination,
        raw[2] as u32,
        FileIoContext::User(user),
    )? as isize)
}

/// 领取消息后只写真实名称、flags 和零 control 长度，后续 fault 不恢复已消费字节。
fn recvmsg(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let file = socket_file(user, raw[0])?;
    let socket = file.socket().expect("checked socket capability");
    let mm = user.mm().expect("recvmsg has no MM");
    let header = message_header(&mm, raw[1])?;
    let mut fast = [UserIoVector {
        address: 0,
        length: 0,
    }; 8];
    let vectors = import_vectors(&mm, header.vectors, header.vector_count, &mut fast)?;
    let mut target = ReadBuffer::user_vector(&mm, &vectors)?;
    let flags = raw[2] as u32;
    let message = socket.receive(&mut target, flags, FileIoContext::User(user))?;
    if header.name != 0 {
        let length = output_name(&mm, header.name, header.name_capacity, message.source)?;
        output_message_field(&mm, raw[1], 8, &(length as u32).to_ne_bytes())?;
    }
    output_message_field(&mm, raw[1], 48, &message.flags.to_ne_bytes())?;
    output_message_field(&mm, raw[1], 40, &0u64.to_ne_bytes())?;
    let count = if socket.socket_type() == SOCK_DGRAM && flags & MSG_TRUNC != 0 {
        message.original_len
    } else {
        message.copied
    };
    Ok(count as isize)
}

/// 消息头输出只覆盖一个规定字段，不改写指针、计数或 padding。
fn output_message_field(mm: &Mm, header: usize, offset: usize, bytes: &[u8]) -> Result<(), Errno> {
    let address = header.checked_add(offset).ok_or(Errno::BadAddress)?;
    copyout(mm, address, bytes)
}

/// 名称容量来自本次快照，短容量只复制地址前缀并返回真实完整长度。
fn output_name(
    mm: &Mm,
    address: usize,
    capacity: usize,
    name: Option<SocketAddress>,
) -> Result<usize, Errno> {
    let mut bytes = [0u8; INET_ADDRESS_LEN];
    let length = name.map_or(0, |name| name.encode(&mut bytes));
    let copied = capacity.min(length);
    if copied != 0 {
        copyout(mm, address, &bytes[..copied])?;
    }
    Ok(length)
}
/// 接收消费真实消息；输入 TRUNC 只选择 UDP 返回原长，名称仍来自该记录。
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
    let socket = file.socket().expect("checked socket capability");
    let count = if socket.socket_type() == SOCK_DGRAM && raw[3] as u32 & MSG_TRUNC != 0 {
        message.original_len
    } else {
        message.copied
    };
    Ok(count as isize)
}

/// 所选布尔选项读取目标 signed int；未知选项和缓冲设置不访问内容。
fn setsockopt(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let file = socket_file(user, raw[0])?;
    let socket = file.socket().expect("checked socket capability");
    let length = raw[4] as i32;
    if length < 0 {
        return Err(Errno::InvalidArgument);
    }
    let level = raw[1] as i32;
    let option = raw[2] as i32;
    match (level, option) {
        (SOL_SOCKET, SO_REUSEADDR) => {}
        (IPPROTO_TCP, TCP_NODELAY) if socket.protocol() == IPPROTO_TCP => {}
        _ => return Err(Errno::ProtocolOptionNotSupported),
    }
    if length < 4 {
        return Err(Errno::InvalidArgument);
    }
    let mm = user.mm().expect("setsockopt has no MM");
    let mut bytes = [0u8; 4];
    copyin(&mm, &mut bytes, raw[3])?;
    let enabled = i32::from_ne_bytes(bytes) != 0;
    if level == SOL_SOCKET {
        socket.set_reuse_address(enabled)?;
    } else {
        socket.set_nodelay(enabled)?;
    }
    Ok(0)
}

/// 查询真实类型、监听状态和缓冲容量；pending error 在交付用户字节前一次消费。
fn getsockopt(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let file = socket_file(user, raw[0])?;
    let socket = file.socket().expect("checked socket capability");
    let level = raw[1] as i32;
    let option = raw[2] as i32;
    if level != SOL_SOCKET
        && !(level == IPPROTO_TCP && option == TCP_NODELAY && socket.protocol() == IPPROTO_TCP)
    {
        return Err(Errno::ProtocolOptionNotSupported);
    }
    let mm = user.mm().expect("getsockopt has no MM");
    let capacity = capacity(&mm, raw[4])?;
    let value = match (level, option) {
        (IPPROTO_TCP, TCP_NODELAY) => i32::from(socket.nodelay()?),
        (SOL_SOCKET, SO_REUSEADDR) => i32::from(socket.reuse_address()),
        (SOL_SOCKET, SO_TYPE) => socket.socket_type() as i32,
        (SOL_SOCKET, SO_ERROR) => socket
            .take_error()
            .map_or(0, |error| Errno::from(error) as i32),
        (SOL_SOCKET, SO_SNDBUF) => {
            i32::try_from(socket.send_capacity()).map_err(|_| Errno::Overflow)?
        }
        (SOL_SOCKET, SO_RCVBUF) => {
            i32::try_from(socket.receive_capacity()).map_err(|_| Errno::Overflow)?
        }
        (SOL_SOCKET, SO_ACCEPTCONN) => i32::from(socket.is_listener()),
        (SOL_SOCKET, SO_PROTOCOL) => socket.protocol(),
        (SOL_SOCKET, SO_DOMAIN) => i32::from(socket.domain()),
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

/// 收发方向关闭直接作用于真实端点，不用关闭描述符冒充 shutdown。
fn shutdown(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let file = socket_file(user, raw[0])?;
    file.socket()
        .expect("checked socket capability")
        .shutdown(raw[1] as u32)?;
    Ok(0)
}
