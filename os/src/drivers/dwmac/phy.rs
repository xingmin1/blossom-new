//! Clause 22 管理总线与两块板实际使用的 Motorcomm PHY。
//! 本地重写管理事务与 PHY 配置，许可与修改声明见本目录 LICENSE。

use crate::{
    boot::{GmacResources, PhyMode},
    drivers::dwmac::{Generation, GmacError, Registers},
    time::{self, Duration},
    timer,
};

const YT8511: u32 = 0x0000_010a;
const YT8531: u32 = 0x4f51_e91b;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Link {
    pub(super) speed: u16,
    pub(super) full_duplex: bool,
}

/// MDIO 只由准备任务或唯一 progress Work 访问，硬中断不获取此对象。
pub(super) struct Phy {
    registers: Registers,
    address: u8,
    clock: u32,
    id: u32,
    adjust_tx: bool,
    inverted: [bool; 3],
}

impl Phy {
    /// 在首次 MAC DMA 复位前配置 PHY；CSR/MDIO 时钟和外设复位须已由板设置启用。
    /// 返回前撤销掉电/隔离并保持无载波时的 RXC，后续运行和停止不得关闭这些输入时钟。
    pub(super) fn prepare(
        registers: Registers,
        resources: &GmacResources,
        csr_divider: u32,
    ) -> Result<Self, GmacError> {
        if 32 <= resources.phy_address || 5 < csr_divider {
            return Err(GmacError::Resource("PHY address or CSR divider is invalid"));
        }
        let mut phy = Self {
            registers,
            address: resources.phy_address as u8,
            clock: csr_divider,
            id: 0,
            adjust_tx: resources.phy_tx_clock_adjust,
            inverted: resources.phy_tx_inverted,
        };
        phy.id = (u32::from(phy.read(2)?) << 16) | u32::from(phy.read(3)?);
        let expected = match registers.generation {
            Generation::Dwmac4 => YT8531,
            Generation::Dwmac1000 => YT8511,
        };
        if phy.id != expected {
            return Err(GmacError::Resource("selected GMAC PHY ID is unsupported"));
        }
        // Clause 22 软复位清除 POWER_DOWN，写入值也明确清除 ISOLATE；
        // 管理时钟由 MAC 的 MDC 提供，复位等待不依赖网线或自协商完成。
        phy.write(0, 1 << 15)?;
        let deadline = time::now().checked_add(Duration::from_millis(500)).unwrap();
        while phy.read(0)? & (1 << 15) != 0 {
            if deadline <= time::now() {
                return Err(GmacError::Io("PHY reset timed out"));
            }
            timer::sleep(Duration::from_millis(1));
        }
        match phy.id {
            YT8531 => phy.configure_yt8531(resources)?,
            YT8511 => phy.configure_yt8511(resources)?,
            _ => unreachable!(),
        }
        phy.advertise()?;
        crate::klog!(
            crate::klog::Level::Info,
            "GMAC PHY id={:#010x} address={} csr_divider={}",
            phy.id,
            phy.address,
            csr_divider
        );
        Ok(phy)
    }

    /// BMSR latch-low 连读两次，只有真实协商完成才返回公共最高能力。
    pub(super) fn sample(&self) -> Result<Option<Link>, GmacError> {
        self.read(1)?;
        let status = self.read(1)?;
        if status == 0 || status == u16::MAX {
            return Err(GmacError::Io("PHY status is unavailable"));
        }
        if status & (1 << 2) == 0 {
            return Ok(None);
        }
        let control = self.read(0)?;
        if control & (1 << 12) == 0 {
            return Err(GmacError::Io("PHY autonegotiation unexpectedly disabled"));
        }
        if status & (1 << 5) == 0 {
            return Ok(None);
        }
        let local = self.read(4)?;
        let partner = self.read(5)?;
        if partner & (1 << 13) != 0 {
            return Err(GmacError::Io("PHY partner reports remote fault"));
        }
        let gigabit = self.read(9)?;
        let peer_gigabit = self.read(10)?;
        if peer_gigabit & (1 << 15) != 0 {
            return Err(GmacError::Io("PHY master slave resolution failed"));
        }
        negotiated(local, partner, gigabit, peer_gigabit)
            .map(Some)
            .ok_or(GmacError::Io("PHY has no common negotiated mode"))
    }

    /// MAC 已暂停后只在协商模式改变时更新 YT8531 的 TX 时钟相位。
    pub(super) fn configure_link(&self, link: Link) -> Result<(), GmacError> {
        if self.adjust_tx {
            let index = match link.speed {
                10 => 0,
                100 => 1,
                1000 => 2,
                _ => unreachable!(),
            };
            self.modify_extended(
                0xa003,
                1 << 14,
                if self.inverted[index] { 1 << 14 } else { 0 },
            )?;
        }
        Ok(())
    }

    /// 仅广告实际 BMSR/扩展状态支持的铜缆模式，未实现的 pause 不对对端宣称。
    fn advertise(&self) -> Result<(), GmacError> {
        let status = self.read(1)?;
        if status & (1 << 3) == 0 {
            return Err(GmacError::Resource("PHY does not support autonegotiation"));
        }
        let advertisement = 1 | ((status >> 6) & 0x01e0);
        if advertisement & 0x01e0 == 0 {
            return Err(GmacError::Resource("PHY has no supported copper mode"));
        }
        self.write(4, (self.read(4)? & !0x0dff) | advertisement)?;
        let extended = if status & (1 << 8) != 0 {
            self.read(15)?
        } else {
            0
        };
        self.write(9, (self.read(9)? & !0x0300) | ((extended >> 4) & 0x0300))?;
        self.write(
            0,
            (self.read(0)? & !((1 << 11) | (1 << 10) | (1 << 14))) | (1 << 12) | (1 << 9),
        )
    }

    /// YT8511 恢复页选择并在睡眠中保持 PLL，RGMII-ID 使用芯片定义的 RX/TX 延时档。
    fn configure_yt8511(&self, resources: &GmacResources) -> Result<(), GmacError> {
        let (rx, tx) = delay_directions(resources.phy_mode)?;
        if resources.rx_delay_ps.is_some()
            || resources.tx_delay_ps.is_some()
            || resources.phy_clock_drive_ua.is_some()
            || resources.phy_data_drive_ua.is_some()
            || resources.phy_tx_clock_adjust
            || resources.phy_tx_inverted.iter().any(|value| *value)
        {
            return Err(GmacError::Resource(
                "YT8511 does not support requested numeric PHY settings",
            ));
        }
        let old = self.read(0x1e)?;
        let result = (|| {
            self.write(0x1e, 0x0c)?;
            let value = self.read(0x1f)?;
            self.write(
                0x1f,
                (value & !0x00f7)
                    | 0x0006
                    | if rx { 1 } else { 0 }
                    | if tx { 0x00f0 } else { 0x0020 },
            )?;
            self.write(0x1e, 0x0d)?;
            self.write(
                0x1f,
                (self.read(0x1f)? & !0xf000) | if tx { 0xf000 } else { 0x2000 },
            )?;
            self.write(0x1e, 0x27)?;
            self.write(0x1f, self.read(0x1f)? | (1 << 14))
        })();
        let restore = self.write(0x1e, old);
        result.and(restore)
    }

    /// YT8531 按实际延时和驱动属性配置，并禁止自动睡眠和无载波时的 RXC 门控。
    fn configure_yt8531(&self, resources: &GmacResources) -> Result<(), GmacError> {
        let (rx_enabled, tx_enabled) = delay_directions(resources.phy_mode)?;
        let (rx, extra) = delay_value(resources.rx_delay_ps.unwrap_or(1950), true)?;
        let (tx, _) = delay_value(resources.tx_delay_ps.unwrap_or(1950), false)?;
        self.modify_extended(0xa001, 1 << 8, if rx_enabled && extra { 1 << 8 } else { 0 })?;
        self.modify_extended(
            0xa003,
            (0xf << 10) | 0xf,
            if rx_enabled { rx << 10 } else { 0 } | if tx_enabled { tx } else { 0 },
        )?;
        let voltage = (self.read_extended(0xa001)? >> 4) & 3;
        let clock = drive_strength(voltage, resources.phy_clock_drive_ua)?;
        let data = drive_strength(voltage, resources.phy_data_drive_ua)?;
        self.modify_extended(
            0xa010,
            (7 << 13) | (1 << 12) | (3 << 4),
            (clock << 13) | ((data & 4) << 10) | ((data & 3) << 4),
        )?;
        // 禁止自动睡眠切断 RX 时钟；链路断开后仍须允许 MAC 的软件复位完成。
        self.modify_extended(0x27, 1 << 15, 0)?;
        self.modify_extended(0x0c, 1 << 12, 0)
    }

    /// Clause 22 读在普通可睡上下文有界等待，不在 IRQ 中轮询。
    fn read(&self, register: u8) -> Result<u16, GmacError> {
        self.transaction(register, None)
    }

    /// Clause 22 写只有 busy 真正清除才结束本次管理事务。
    fn write(&self, register: u8, value: u16) -> Result<(), GmacError> {
        self.transaction(register, Some(value)).map(|_| ())
    }

    /// 两代管理地址字段和 GOC/MW 编码分开，MDC 分频来自已验证的输入率。
    fn transaction(&self, register: u8, value: Option<u16>) -> Result<u16, GmacError> {
        let (address, data, command) = match self.registers.generation {
            Generation::Dwmac4 => (
                0x200,
                0x204,
                (u32::from(self.address) << 21)
                    | (u32::from(register) << 16)
                    | (self.clock << 8)
                    | if value.is_some() { 1 << 2 } else { 3 << 2 },
            ),
            Generation::Dwmac1000 => (
                0x10,
                0x14,
                (u32::from(self.address) << 11)
                    | (u32::from(register) << 6)
                    | (self.clock << 2)
                    | if value.is_some() { 1 << 1 } else { 0 },
            ),
        };
        self.wait_idle(address)?;
        if let Some(value) = value {
            self.registers.write(data, u32::from(value));
        }
        self.registers.write(address, command | 1);
        self.wait_idle(address)?;
        Ok(self.registers.read(data) as u16)
    }

    /// 单事务超时报告真实 MDIO 故障，不返回伪造的全零 PHY 状态。
    fn wait_idle(&self, offset: usize) -> Result<(), GmacError> {
        let deadline = time::now().checked_add(Duration::from_millis(20)).unwrap();
        while self.registers.read(offset) & 1 != 0 {
            if deadline <= time::now() {
                return Err(GmacError::Io("MDIO transaction timed out"));
            }
            timer::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    /// 扩展地址锁由唯一管理执行者承担，选择与数据读不被其它 MDIO 事务穿插。
    fn read_extended(&self, register: u16) -> Result<u16, GmacError> {
        self.write(0x1e, register)?;
        self.read(0x1f)
    }

    /// 只修改所需厂商位，其余 strap 与电气设置保持原值。
    fn modify_extended(&self, register: u16, mask: u16, value: u16) -> Result<(), GmacError> {
        let current = self.read_extended(register)?;
        self.write(0x1e, register)?;
        self.write(0x1f, (current & !mask) | value)
    }
}

/// 六种标准 CSR 范围确保 MDC 不超过 2.5MHz，未支持的输入率保资源错误。
pub(super) fn clock_range(frequency: u64) -> Result<u32, GmacError> {
    match frequency {
        20_000_000..35_000_000 => Ok(2),
        35_000_000..60_000_000 => Ok(3),
        60_000_000..100_000_000 => Ok(0),
        100_000_000..150_000_000 => Ok(1),
        150_000_000..250_000_000 => Ok(4),
        250_000_000..=300_000_000 => Ok(5),
        _ => Err(GmacError::Resource(
            "MAC CSR clock is outside supported MDIO ranges",
        )),
    }
}

/// 接口模式指定由 PHY 加延时的方向，板修订不由驱动猜测。
fn delay_directions(mode: PhyMode) -> Result<(bool, bool), GmacError> {
    match mode {
        PhyMode::Rgmii => Ok((false, false)),
        PhyMode::RgmiiId => Ok((true, true)),
        PhyMode::RgmiiRxid => Ok((true, false)),
        PhyMode::RgmiiTxid => Ok((false, true)),
        PhyMode::Rmii => Err(GmacError::Resource("selected PHY requires RGMII")),
    }
}

/// 非法数值不沿最近档位取整；额外 RX 位只用于可精确表示的 1900ps 偏置。
const fn delay_value(picoseconds: u32, receive: bool) -> Result<(u16, bool), GmacError> {
    if picoseconds <= 2250 && picoseconds % 150 == 0 {
        Ok(((picoseconds / 150) as u16, false))
    } else if receive
        && 1900 <= picoseconds
        && picoseconds <= 4150
        && (picoseconds - 1900) % 150 == 0
    {
        Ok((((picoseconds - 1900) / 150) as u16, true))
    } else {
        Err(GmacError::Resource("YT8531 delay cannot be represented"))
    }
}

/// 电流档由芯片实际 LDO strap 选择，缺省采用该 PHY 定义的驱动档3。
fn drive_strength(voltage: u16, microamps: Option<u32>) -> Result<u16, GmacError> {
    let Some(microamps) = microamps else {
        return Ok(3);
    };
    let values = match voltage {
        0 => [3070, 4080, 4370, 4680, 5020, 5450, 5740, 6140],
        2 => [1200, 2100, 2700, 2910, 3110, 3600, 3970, 4350],
        _ => {
            return Err(GmacError::Resource(
                "YT8531 LDO voltage strap is unsupported",
            ))
        }
    };
    values
        .iter()
        .position(|value| *value == microamps)
        .map(|index| index as u16)
        .ok_or(GmacError::Resource(
            "YT8531 drive current cannot be represented",
        ))
}

/// Clause 22 的共同模式按 1000/100/10 与全双工优先顺序选择。
const fn negotiated(local: u16, partner: u16, gigabit: u16, peer_gigabit: u16) -> Option<Link> {
    let common = local & partner;
    let gigabit_common = gigabit & (peer_gigabit >> 2);
    let (speed, full_duplex) = if gigabit_common & (1 << 9) != 0 {
        (1000, true)
    } else if gigabit_common & (1 << 8) != 0 {
        (1000, false)
    } else if common & (1 << 8) != 0 {
        (100, true)
    } else if common & (1 << 7) != 0 {
        (100, false)
    } else if common & (1 << 6) != 0 {
        (10, true)
    } else if common & (1 << 5) != 0 {
        (10, false)
    } else {
        return None;
    };
    Some(Link { speed, full_duplex })
}

// 纯协议检查随编译执行，不需要实板或伪造管理寄存器。
const _: () = {
    assert!(matches!(delay_value(1900, true), Ok((0, true))));
    assert!(matches!(delay_value(300, false), Ok((2, false))));
    assert!(delay_value(1900, false).is_err());
    assert!(delay_value(1499, true).is_err());
    assert!(matches!(
        negotiated(0x01e1, 0x01e1, 0x0200, 0x0800),
        Some(Link {
            speed: 1000,
            full_duplex: true
        })
    ));
    assert!(matches!(
        negotiated(0x01e1, 0x0081, 0, 0),
        Some(Link {
            speed: 100,
            full_duplex: false
        })
    ));
    assert!(negotiated(1, 1, 0, 0).is_none());
};
