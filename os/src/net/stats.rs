//! 接口计数只累计实际帧事件，查询取得无拥有的小值副本。

/// 八项实际 RX/TX 计数；未监测的硬件错误项保持零。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InterfaceStats {
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub rx_errors: u64,
    pub rx_dropped: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
    pub tx_errors: u64,
    pub tx_dropped: u64,
}
