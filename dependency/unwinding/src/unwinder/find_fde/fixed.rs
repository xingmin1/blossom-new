use super::FDESearchResult;

use gimli::{BaseAddresses, EhFrame, NativeEndian, UnwindSection};

pub struct StaticFinder(());

/// 静态内核只读展开表使用进程内唯一 finder，不进行动态分配。
pub fn get_finder() -> &'static StaticFinder {
    &StaticFinder(())
}

unsafe extern "C" {
    static __executable_start: u8;
    static __etext: u8;
    static __eh_frame: u8;
    static __eh_frame_end: u8;
}

impl super::FDEFinder for StaticFinder {
    /// 在链接器保留的可执行 VMA 和完整 eh_frame 范围内线性查找 FDE。
    fn find_fde(&self, pc: usize) -> Option<FDESearchResult> {
        // SAFETY: 链接器提供永久映射的只读段及其末尾；符号地址均为执行 VMA。
        unsafe {
            let text_start = &__executable_start as *const u8 as usize;
            let text_end = &__etext as *const u8 as usize;
            if !(text_start..text_end).contains(&pc) {
                return None;
            }

            let eh_frame = &__eh_frame as *const u8 as usize;
            let bases = BaseAddresses::default()
                .set_eh_frame(eh_frame as _)
                .set_text(text_start as _);
            let end = core::ptr::addr_of!(__eh_frame_end) as usize;
            let length = end.checked_sub(eh_frame)?;
            let bytes = core::slice::from_raw_parts(eh_frame as *const u8, length);
            let eh_frame = EhFrame::new(bytes, NativeEndian);

            if let Ok(fde) = eh_frame.fde_for_address(&bases, pc as _, EhFrame::cie_from_offset) {
                return Some(FDESearchResult {
                    fde,
                    bases,
                    eh_frame,
                });
            }

            None
        }
    }
}
