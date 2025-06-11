use super::BlockDevice;
use crate::mm::{
    frame_alloc, frames_alloc, frame_dealloc, kernel_token, FrameTracker, PageTable, PageTableImpl, PhysAddr,
    PhysPageNum, StepByOne, VirtAddr,
};
use alloc::{sync::Arc, vec::Vec};
use core::ptr::NonNull;
use lazy_static::*;
use spin::Mutex;
use virtio_drivers::device::blk::VirtIOBlk;
use virtio_drivers::transport::mmio::{VirtIOHeader,MmioTransport}; 
use virtio_drivers::{BufferDirection, Hal};
const VIRT_IO_BLOCK_SZ: usize = 512;
use crate::hal::config::{BLOCK_SZ, PAGE_SIZE, PAGE_SIZE_BITS};
const BLOCK_RATIO: usize = BLOCK_SZ / VIRT_IO_BLOCK_SZ;
#[allow(unused)]
const VIRTIO0: usize = 0x10001000;

pub struct VirtIOBlock(Mutex<VirtIOBlk<VirtioHal, MmioTransport<'static>>>);

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

impl VirtIOBlock {
    #[allow(unused)]
    pub fn new() -> Self {
        unsafe {
            Self(Mutex::new(
                VirtIOBlk::<VirtioHal, MmioTransport<'static>>::new(
                    MmioTransport::new(
                        NonNull::new_unchecked((VIRTIO0) as *mut VirtIOHeader),
                        0x1000,
                    )
                    .expect("this is not a valid virtio device"),
                )
                .unwrap(),
            ))
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

#[no_mangle]
pub extern "C" fn virtio_dma_alloc(pages: usize) -> PhysAddr {
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

#[no_mangle]
pub extern "C" fn virtio_dma_dealloc(pa: PhysAddr, pages: usize) -> i32 {
    let mut ppn_base: PhysPageNum = pa.into();
    for _ in 0..pages {
        frame_dealloc(ppn_base);
        ppn_base.step();
    }
    0
}

#[no_mangle]
pub extern "C" fn virtio_phys_to_virt(paddr: PhysAddr) -> VirtAddr {
    VirtAddr(paddr.0)
}

lazy_static! {
    static ref KERNEL_TOKEN: usize = kernel_token();
}

#[no_mangle]
pub extern "C" fn virtio_virt_to_phys(vaddr: VirtAddr) -> PhysAddr {
    PageTableImpl::from_token(*KERNEL_TOKEN)
        .translate_va(vaddr)
        .unwrap()
}
