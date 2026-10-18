//! 32 位 DW-MMC 寄存器与命令位；MMIO 的映射和独占由控制器拥有者证明。

pub(super) const CTRL: usize = 0x00;
pub(super) const PWREN: usize = 0x04;
pub(super) const CLKDIV: usize = 0x08;
pub(super) const CLKSRC: usize = 0x0c;
pub(super) const CLKENA: usize = 0x10;
pub(super) const TMOUT: usize = 0x14;
pub(super) const CTYPE: usize = 0x18;
pub(super) const BLKSIZ: usize = 0x1c;
pub(super) const BYTCNT: usize = 0x20;
pub(super) const INTMASK: usize = 0x24;
pub(super) const CMDARG: usize = 0x28;
pub(super) const CMD: usize = 0x2c;
pub(super) const RESP0: usize = 0x30;
pub(super) const MINTSTS: usize = 0x40;
pub(super) const RINTSTS: usize = 0x44;
pub(super) const STATUS: usize = 0x48;
pub(super) const FIFOTH: usize = 0x4c;
pub(super) const CDETECT: usize = 0x50;
pub(super) const WRTPRT: usize = 0x54;
pub(super) const TCBCNT: usize = 0x5c;
pub(super) const TBBCNT: usize = 0x60;
pub(super) const VERID: usize = 0x6c;
pub(super) const HCON: usize = 0x70;
pub(super) const UHS: usize = 0x74;
pub(super) const BMOD: usize = 0x80;
pub(super) const PLDMND: usize = 0x84;
pub(super) const DBADDR: usize = 0x88;
pub(super) const IDSTS: usize = 0x8c;
pub(super) const IDINTEN: usize = 0x90;

pub(super) const RESET: u32 = 7;
pub(super) const FIFO_RESET: u32 = 1 << 1;
pub(super) const DMA_RESET: u32 = 1 << 2;
pub(super) const INT_ENABLE: u32 = 1 << 4;
pub(super) const DMA_ENABLE: u32 = 1 << 5;
pub(super) const USE_IDMAC: u32 = 1 << 25;
pub(super) const BMOD_RESET: u32 = 1;
pub(super) const BMOD_ENABLE: u32 = 1 << 7;
pub(super) const DATA_BUSY: u32 = 1 << 9;
pub(super) const DMA_REQ: u32 = 1 << 31;
pub(super) const CMD_DONE: u32 = 1 << 2;
pub(super) const DATA_OVER: u32 = 1 << 3;
pub(super) const RESPONSE_TIMEOUT: u32 = 1 << 8;
pub(super) const ERRORS: u32 = 0xbfc2;
pub(super) const IRQ_MASK: u32 = ERRORS | CMD_DONE | DATA_OVER;
pub(super) const IDMAC_DONE: u32 = 1 << 30;
pub(super) const IDMAC_ERROR: u32 = 1 << 31;
pub(super) const IDMAC_ERRORS: u32 = (1 << 2) | (1 << 4) | (1 << 5) | (1 << 9);
pub(super) const IDMAC_IRQ: u32 = IDMAC_ERRORS | (1 << 8) | 3;
pub(super) const START: u32 = 1 << 31;
pub(super) const HOLD: u32 = 1 << 29;
pub(super) const UPDATE_CLOCK: u32 = 1 << 21;
pub(super) const INITIALIZE: u32 = 1 << 15;
pub(super) const STOP: u32 = 1 << 14;
pub(super) const WAIT_DATA: u32 = 1 << 13;
pub(super) const WRITE: u32 = 1 << 10;
pub(super) const DATA: u32 = 1 << 9;
pub(super) const CRC: u32 = 1 << 8;
pub(super) const LONG: u32 = 1 << 7;
pub(super) const RESPONSE: u32 = 1 << 6;

/// 验证后的永久 MMIO 窗口；方法只接受本模块公开的对齐偏移。
pub(super) struct Registers(pub usize);

impl Registers {
    /// 读取独占映射内的寄存器；并发 W1C 和控制更新由外层短锁协调。
    pub fn read(&self, offset: usize) -> u32 {
        // SAFETY: prepare 验证整个寄存器窗口，拥有者保活映射；偏移只来自本模块常量。
        unsafe { ((self.0 + offset) as *const u32).read_volatile() }
    }

    /// 写入独占映射内的寄存器，不把普通原子操作当设备访问。
    pub fn write(&self, offset: usize, value: u32) {
        // SAFETY: prepare 验证整个寄存器窗口，拥有者保活映射；偏移只来自本模块常量。
        unsafe { ((self.0 + offset) as *mut u32).write_volatile(value) }
    }

    /// 在调用方持有寄存器短锁或尚未发表时更新控制位。
    pub fn update(&self, offset: usize, clear: u32, set: u32) {
        self.write(offset, (self.read(offset) & !clear) | set);
    }

    /// 读取 R2 的低字在前原始排列。
    pub fn response(&self) -> [u32; 4] {
        core::array::from_fn(|index| self.read(RESP0 + index * 4))
    }
}
