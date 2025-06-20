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
//use log::debug;
use virtio_drivers::transport::pci::{PciTransport,virtio_device_type};
use virtio_drivers::device::blk::VirtIOBlk;
use virtio_drivers::transport::DeviceType;
const VIRT_IO_BLOCK_SZ: usize = 512;
use crate::hal::config::{BLOCK_SZ, PAGE_SIZE, PAGE_SIZE_BITS};
const BLOCK_RATIO: usize = BLOCK_SZ / VIRT_IO_BLOCK_SZ;
//#[allow(unused)]
//#[cfg(target_arch = "loongarch64")]
const VIRTIO0: usize = 0x1000_0000;
const VIRT_PCI_BASE: usize = 0x400E_0000;
const VIRT_PCI_SIZE: usize = 0x0001_0000;


pub struct VirtIOBlock(Mutex<VirtIOBlk<VirtioHal, PciTransport>>);

lazy_static! {
    static ref QUEUE_FRAMES: Mutex<Vec<Arc<FrameTracker>>> = Mutex::new(Vec::new());
}

impl BlockDevice for VirtIOBlock {
    fn read_block(&self, block_id: usize, buf: &mut [u8]) {
        assert!(
            buf.len() % BLOCK_SZ == 0,
            "Buffer size must be multiple of BLOCK_SZ"
        );
        // log::info!("Reading block {} with size {}", block_id, buf.len());
        // for buf in buf.chunks_mut(VIRT_IO_BLOCK_SZ) {
        //     self.0
        //         .lock()
        //         .read_block(block_id, buf)
        //         .expect("Error when reading VirtIOBlk");
        //     block_id += 1;
        for (i, chunk) in buf.chunks_mut(VIRT_IO_BLOCK_SZ).enumerate() {
            let virtio_block_id = block_id * BLOCK_RATIO + i;
            self.0
                .lock()
                .read_blocks(virtio_block_id as usize, chunk)
                .expect("Error when reading VirtIOBlk");
        }
    }
    fn write_block(&self, block_id: usize, buf: &[u8]) {
        assert!(
            buf.len() % BLOCK_SZ == 0,
            "Buffer size must be multiple of BLOCK_SZ"
        );
        for (i, chunk) in buf.chunks(VIRT_IO_BLOCK_SZ).enumerate() {
            let virtio_block_id = block_id * BLOCK_RATIO + i;
            self.0
                .lock()
                .write_blocks(virtio_block_id as usize, chunk)
                .expect("Error when writing VirtIOBlk");
        }
        // for buf in buf.chunks(VIRT_IO_BLOCK_SZ) {
        //     self.0
        //         .lock()
        //         .write_block(block_id, buf)
        //         .expect("Error when writing VirtIOBlk");
        //     block_id += 1;
        // }
    }
}

pub struct PciRangeAllocator {
    _start: usize,
    end: usize,
    current: usize,

}

impl PciRangeAllocator {
    /// Creates a new allocator from a memory range.
    pub const fn new(pci_base:usize,pci_size:usize) -> Self {
        Self {
            _start:pci_base,
            end:pci_base+pci_size,
            current:pci_base
        }
    }
    pub fn alloc_pci_mem(&mut self,size: usize) -> Option<usize> {
        if !size.is_power_of_two() {
            return None;
        }
        let ret = align_up(self.current, size);
        if ret + size > self.end {
            return None;
        }
        self.current = ret + size;
        Some(ret)
    }
}


const fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}



fn enumerate_pci()->Option<PciTransport> {
    let mmconfig_base = VIRTIO0 as *mut u8;
    
    let mmio_cam = unsafe { MmioCam::new(mmconfig_base, Cam::Ecam) };
    let mut pci_root = PciRoot::new(mmio_cam);

    let mut transport = None;

    for (device_function, info) in pci_root.enumerate_bus(0) {
        let (_status, _command) = pci_root.get_status_command(device_function);

        if let Some(_virtio_type) = virtio_device_type(&info) {
            if _virtio_type != DeviceType::Block {continue;}
            let mut pci_range_allocator = PciRangeAllocator::new(VIRT_PCI_BASE, VIRT_PCI_SIZE);
            let mut bar_index = 0;
            while bar_index < 6 {
                if let Some(BarInfo::Memory {address_type, address, size, ..}) = pci_root.bar_info(device_function, bar_index).unwrap() {
                    if address == 0 && size != 0 {
                        let alloc_addr = pci_range_allocator.alloc_pci_mem(size as usize).unwrap();
                        match address_type {
                            MemoryBarType::Width64 => pci_root.set_bar_64(device_function, bar_index, alloc_addr as u64),
                            MemoryBarType::Width32 => pci_root.set_bar_32(device_function, bar_index, alloc_addr as u32),
                            _ => {}
                        }
                    }
                }
                bar_index += 1;
                if let Some(bar_info) = pci_root.bar_info(device_function, bar_index).unwrap() {
                    if bar_info.takes_two_entries() {
                        bar_index += 1;
                    }
                }
            }

            // Enable the device to use its BARs.
            pci_root.set_command(
                device_function,
                Command::IO_SPACE | Command::MEMORY_SPACE | Command::BUS_MASTER,
            );
         //   dump_bar_contents(&mut pci_root, device_function, 1);

            transport = 
                Some(PciTransport::new::<VirtioHal, MmioCam>(&mut pci_root, device_function).unwrap());
            break;
        }
    }
    transport
}
#[allow(unused)]
fn dump_bar_contents(
    root: &mut PciRoot<MmioCam>,
    device_function: DeviceFunction,
    bar_index: u8,
) {
    if let Some(BarInfo::Memory { address, size, .. }) = root.bar_info(device_function, bar_index).unwrap() {
        let start = address as *const u8;
        println!("start:{:?}",start);
        unsafe {
            let mut buf = [0u8; 32];
            for i in 0..size / 32 {
                let ptr = start.add(i as usize * 32);
                println!("ptr:{:?}",ptr);
                core::ptr::copy(ptr, buf.as_mut_ptr(), 32);
                if buf.iter().any(|b| *b != 0xff) {
                }
            }
        }
    }
}

impl VirtIOBlock {
    #[allow(unused)]
    pub fn new() -> Self {
        unsafe {
            Self(Mutex::new(
                VirtIOBlk::<VirtioHal, PciTransport>::new(
                    enumerate_pci().unwrap()
                    )
                    .expect("this is not a valid virtio device"),
                )
            )
        }
    }
}

pub struct VirtioHal;

unsafe impl Hal for VirtioHal {
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (usize, NonNull<u8>) {
        //log::info!("use dma_alloc with pages: {}", pages);
        let paddr = virtio_dma_alloc(pages);
        let vaddr = virtio_phys_to_virt(paddr);
        let ptr = NonNull::new(vaddr.0 as *mut u8)
            .expect("virtio_phys_to_virt returned null pointer in dma_alloc");
        (paddr.0, ptr)
    }

    unsafe fn dma_dealloc(paddr: usize, _vaddr: NonNull<u8>, pages: usize) -> i32 {
        //log::info!("use dma_dealloc with paddr: {}, pages: {}", paddr, pages);
        virtio_dma_dealloc(PhysAddr(paddr), pages)
    }

    unsafe fn mmio_phys_to_virt(paddr: usize, _size: usize) -> NonNull<u8> {
        //log::info!("use mmio_phys_to_virt with paddr: {}", paddr);
        let vaddr = virtio_phys_to_virt(PhysAddr(paddr));
        NonNull::new(vaddr.0 as *mut u8)
            .expect("virtio_phys_to_virt returned null pointer in mmio_phys_to_virt")
    }

    unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection) -> usize {
        let buffer = buffer.as_ref();
        let pages = (buffer.len() + PAGE_SIZE - 1) >> PAGE_SIZE_BITS;
        let frames = frames_alloc(pages).expect("share: failed to alloc frames");
        
        if matches!(direction, BufferDirection::DriverToDevice | BufferDirection::Both) {
            // 获取第一个物理页的起始地址作为连续区域的基址
            let pa_start = frames[0].ppn.start_addr().0;
            // 直接复制到物理内存
            let dst_slice = core::slice::from_raw_parts_mut(pa_start as *mut u8, buffer.len());
            dst_slice.copy_from_slice(buffer);
        }

        let pa = frames[0].ppn.start_addr().0;
        QUEUE_FRAMES.lock().extend(frames);
        pa
    }

    unsafe fn unshare(paddr: usize, mut buffer: NonNull<[u8]>, direction: BufferDirection) {
            let buffer = buffer.as_mut();
            let ppn_start = PhysAddr(paddr).floor();
            let ppn_end = PhysAddr(paddr + buffer.len()).ceil();
    
            if matches!(direction, BufferDirection::DeviceToDriver | BufferDirection::Both) {
                let src_ptr = paddr as *const u8;
                buffer.copy_from_slice(core::slice::from_raw_parts(src_ptr, buffer.len()));
            }
        let mut current_ppn = ppn_start;
        while current_ppn != ppn_end {
            frame_dealloc(current_ppn);
            current_ppn.step();
        }
    }
}

pub  fn virtio_dma_alloc(pages: usize) -> PhysAddr {
    let mut ppn_base = PhysPageNum(0);
    for i in 0..pages {
        let frame = frame_alloc().unwrap();
        if i == 0 {
            ppn_base = frame.ppn;
        }
        assert_eq!(frame.ppn.0, ppn_base.0 + i);
        QUEUE_FRAMES.lock().push(frame);
    }
    ppn_base.into()
}

pub  fn virtio_dma_dealloc(pa: PhysAddr, pages: usize) -> i32 {
    let mut ppn_base: PhysPageNum = pa.into();
    for _ in 0..pages {
        frame_dealloc(ppn_base);
        ppn_base.step();
    }
    0
}

pub  fn virtio_phys_to_virt(paddr: PhysAddr) -> VirtAddr {
    VirtAddr(paddr.0)
}

lazy_static! {
    static ref KERNEL_TOKEN: usize = kernel_token();
}

pub fn virtio_virt_to_phys(vaddr: VirtAddr) -> PhysAddr {
    PageTableImpl::from_token(*KERNEL_TOKEN)
        .translate_va(vaddr)
        .unwrap()
}
