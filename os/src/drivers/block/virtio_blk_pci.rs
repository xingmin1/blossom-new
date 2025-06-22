use super::BlockDevice;
use crate::mm::{
    frame_alloc, frames_alloc, frame_dealloc, kernel_token, FrameTracker, PageTable, PageTableImpl, PhysAddr,
    PhysPageNum, StepByOne, VirtAddr,
};
use spin::Mutex;
use alloc::vec::Vec;
use alloc::sync::Arc;
use lazy_static::*;
use core::ptr::NonNull;
use virtio_drivers::{BufferDirection, Hal};
use virtio_drivers::transport::pci::bus::{BarInfo, Cam, Command, DeviceFunction, MemoryBarType, PciRoot, MmioCam};
use virtio_drivers::transport::pci::{PciTransport, virtio_device_type};
use virtio_drivers::device::blk::VirtIOBlk;
use virtio_drivers::transport::DeviceType;
const VIRT_IO_BLOCK_SZ: usize = 512;
use crate::hal::config::{BLOCK_SZ, PAGE_SIZE, PAGE_SIZE_BITS};
const BLOCK_RATIO: usize = BLOCK_SZ / VIRT_IO_BLOCK_SZ;
const PCI_ECAM_BASE: usize = 0x2000_0000;
const VIRT_PCI_BASE: usize = 0x4000_0000;
const VIRT_PCI_SIZE: usize = 0x0002_0000;

pub struct VirtIOBlock(Mutex<VirtIOBlk<VirtioHal, PciTransport>>);

lazy_static! {
    static ref QUEUE_FRAMES: Mutex<Vec<Arc<FrameTracker>>> = Mutex::new(Vec::new());
}

impl BlockDevice for VirtIOBlock {
    fn read_block(&self, block_id: usize, buf: &mut [u8]) {
        assert!(buf.len() % BLOCK_SZ == 0);
        for (i, chunk) in buf.chunks_mut(VIRT_IO_BLOCK_SZ).enumerate() {
            let virtio_block_id = block_id * BLOCK_RATIO + i;
            self.0.lock().read_blocks(virtio_block_id, chunk).expect("read error");
        }
    }

    fn write_block(&self, block_id: usize, buf: &[u8]) {
        assert!(buf.len() % BLOCK_SZ == 0);
        for (i, chunk) in buf.chunks(VIRT_IO_BLOCK_SZ).enumerate() {
            let virtio_block_id = block_id * BLOCK_RATIO + i;
            self.0.lock().write_blocks(virtio_block_id, chunk).expect("write error");
        }
    }
}

pub struct PciRangeAllocator {
    end: usize,
    current: usize,
}

impl PciRangeAllocator {
    pub const fn new(pci_base: usize, pci_size: usize) -> Self {
        Self { current: pci_base, end: pci_base + pci_size }
    }

    pub fn alloc_pci_mem(&mut self, size: usize) -> Option<usize> {
        if !size.is_power_of_two() { return None; }
        let ret = align_up(self.current, size);
        if ret + size > self.end { return None; }
        self.current = ret + size;
        Some(ret & !0xf)
    }
}

const fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

fn enumerate_pci() -> Option<PciTransport> {
    let mmconfig_base = PCI_ECAM_BASE as *mut u8;
    println!("[PCI] ECAM base: {:#x}", mmconfig_base as usize);

    let mmio_cam = unsafe { MmioCam::new(mmconfig_base, Cam::Ecam) };
    let mut pci_root = PciRoot::new(mmio_cam);
    let mut transport = None;

    for (device_function, info) in pci_root.enumerate_bus(0) {
        println!("[PCI] Device {:?}: vendor={:#x} device={:#x}", device_function, info.vendor_id, info.device_id);
        if let Some(virtio_type) = virtio_device_type(&info) {
            println!("[PCI] VirtIO device: {:?}", virtio_type);
            if virtio_type != DeviceType::Block { continue; }

            println!("[PCI] Configuring BARs...");
            let mut allocator = PciRangeAllocator::new(VIRT_PCI_BASE, VIRT_PCI_SIZE);
            let mut bar_index = 0;
            while bar_index < 6 {
                if let Some(bar) = pci_root.bar_info(device_function, bar_index).unwrap() {
                    if let BarInfo::Memory { address_type, address, size, .. } = bar {
                        println!("[PCI] BAR{}: {:?}, addr={:#x}, size={:#x}", bar_index, address_type, address, size);
                        if address == 0 && size != 0 {
                            if let Some(alloc_addr) = allocator.alloc_pci_mem(size as usize) {
                                match address_type {
                                    MemoryBarType::Width64 => pci_root.set_bar_64(device_function, bar_index, alloc_addr as u64),
                                    MemoryBarType::Width32 => pci_root.set_bar_32(device_function, bar_index, alloc_addr as u32),
                                    _ => {}
                                }
                            }
                        }
                    }
                    if bar.takes_two_entries() {
                        println!("[PCI] BAR{} is 64-bit", bar_index);
                        bar_index += 1;
                    }
                }
                bar_index += 1;
            }

            pci_root.set_command(device_function, Command::IO_SPACE | Command::MEMORY_SPACE | Command::BUS_MASTER);
            println!("[PCI] Device enabled.");
            transport = Some(PciTransport::new::<VirtioHal, MmioCam>(&mut pci_root, device_function).unwrap());
            break;
        }
    }
    transport
}

impl VirtIOBlock {
    pub fn new() -> Self {
        Self(Mutex::new(
            VirtIOBlk::<VirtioHal, PciTransport>::new(
                enumerate_pci().expect("No VirtIO block device")
            ).expect("Invalid VirtIO device")
        ))
    }
}

pub struct VirtioHal;

unsafe impl Hal for VirtioHal {
    fn dma_alloc(pages: usize, _dir: BufferDirection) -> (usize, NonNull<u8>) {
        let paddr = virtio_dma_alloc(pages);
        let vaddr = virtio_phys_to_virt(paddr);
        let ptr = NonNull::new(vaddr.0 as *mut u8).unwrap();
        (paddr.0, ptr)
    }

    unsafe fn dma_dealloc(paddr: usize, _vaddr: NonNull<u8>, pages: usize) -> i32 {
        virtio_dma_dealloc(PhysAddr(paddr), pages)
    }

    unsafe fn mmio_phys_to_virt(paddr: usize, _size: usize) -> NonNull<u8> {
        let vaddr = virtio_phys_to_virt(PhysAddr(paddr));
        NonNull::new(vaddr.0 as *mut u8).unwrap()
    }

    unsafe fn share(buffer: NonNull<[u8]>, dir: BufferDirection) -> usize {
        let buffer = buffer.as_ref();
        let pages = (buffer.len() + PAGE_SIZE - 1) >> PAGE_SIZE_BITS;
        let frames = frames_alloc(pages).unwrap();
        if matches!(dir, BufferDirection::DriverToDevice | BufferDirection::Both) {
            let pa_start = frames[0].ppn.start_addr().0;
            core::slice::from_raw_parts_mut(pa_start as *mut u8, buffer.len()).copy_from_slice(buffer);
        }
        let pa = frames[0].ppn.start_addr().0;
        QUEUE_FRAMES.lock().extend(frames);
        pa
    }

    unsafe fn unshare(paddr: usize, mut buffer: NonNull<[u8]>, dir: BufferDirection) {
        let buffer = buffer.as_mut();
        if matches!(dir, BufferDirection::DeviceToDriver | BufferDirection::Both) {
            let src = paddr as *const u8;
            buffer.copy_from_slice(core::slice::from_raw_parts(src, buffer.len()));
        }
        let mut ppn = PhysAddr(paddr).floor();
        let end = PhysAddr(paddr + buffer.len()).ceil();
        while ppn != end {
            frame_dealloc(ppn);
            ppn.step();
        }
    }
}

pub fn virtio_dma_alloc(pages: usize) -> PhysAddr {
    let mut ppn_base = PhysPageNum(0);
    for i in 0..pages {
        let frame = frame_alloc().unwrap();
        if i == 0 { ppn_base = frame.ppn; }
        assert_eq!(frame.ppn.0, ppn_base.0 + i);
        QUEUE_FRAMES.lock().push(frame);
    }
    ppn_base.into()
}

pub fn virtio_dma_dealloc(pa: PhysAddr, pages: usize) -> i32 {
    let mut ppn = pa.into();
    for _ in 0..pages {
        frame_dealloc(ppn);
        ppn.step();
    }
    0
}

pub fn virtio_phys_to_virt(paddr: PhysAddr) -> VirtAddr {
    VirtAddr(paddr.0)
}

lazy_static! {
    static ref KERNEL_TOKEN: usize = kernel_token();
}

pub fn virtio_virt_to_phys(vaddr: VirtAddr) -> PhysAddr {
    PageTableImpl::from_token(*KERNEL_TOKEN).translate_va(vaddr).unwrap()
}
