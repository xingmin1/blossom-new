//! 路由只从真实本机配置选择源地址，不修改通配绑定。

use crate::fs::FsError;
use crate::net::LOOPBACK_MTU;

/// 当前路径选定的源地址和完整 IPv4 MTU。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Route {
    pub source: [u8; 4],
    pub mtu: usize,
}

/// 唯一本地接口持实际地址与前缀，不含尚未存在的外部接口。
pub struct RouteTable {
    loopback: Route,
}

impl RouteTable {
    /// 建立 lo 的 127.0.0.1/8 与 1500 字节 MTU，不产生异步工作。
    pub fn new_loopback() -> Self {
        Self {
            loopback: Route {
                source: [127, 0, 0, 1],
                mtu: LOOPBACK_MTU,
            },
        }
    }

    /// lo 的整个 127/8 前缀均按本机目的地址处理。
    pub fn is_local(&self, address: [u8; 4]) -> bool {
        address[0] == self.loopback.source[0]
    }

    /// 先核具体绑定源，再找目的路径；通配仅在返回值中选择实际 lo 源。
    pub fn resolve(&self, destination: [u8; 4], bound_source: [u8; 4]) -> Result<Route, FsError> {
        if bound_source != [0; 4] && !self.is_local(bound_source) {
            return Err(FsError::AddressNotAvailable);
        }
        if !self.is_local(destination) {
            return Err(FsError::NetworkUnreachable);
        }
        Ok(Route {
            source: if bound_source == [0; 4] {
                self.loopback.source
            } else {
                bound_source
            },
            mtu: self.loopback.mtu,
        })
    }
}
