// References:
// https://github.com/rust-lang/rust/blob/c4be230b4a30eb74e3a3908455731ebc2f731d3d/library/panic_unwind/src/gcc.rs
// https://github.com/rust-lang/rust/blob/c4be230b4a30eb74e3a3908455731ebc2f731d3d/library/panic_unwind/src/dwarf/eh.rs
// https://docs.rs/gimli/0.25.0/src/gimli/read/cfi.rs.html

use core::mem;
use gimli::{constants, NativeEndian};
use gimli::{EndianSlice, Error, Pointer, Reader};

use crate::abi::*;
use crate::arch::*;
use crate::util::*;

#[derive(Debug)]
enum EHAction {
    None,
    Cleanup(usize),
    Catch(usize),
    Filter(usize),
    Terminate,
}

fn parse_pointer_encoding(input: &mut StaticSlice) -> gimli::Result<constants::DwEhPe> {
    let eh_pe = input.read_u8()?;
    let eh_pe = constants::DwEhPe(eh_pe);

    if eh_pe.is_valid_encoding() {
        Ok(eh_pe)
    } else {
        Err(gimli::Error::UnknownPointerEncoding(eh_pe))
    }
}

fn parse_encoded_pointer(
    encoding: constants::DwEhPe,
    unwind_ctx: &UnwindContext<'_>,
    input: &mut StaticSlice,
) -> gimli::Result<Pointer> {
    if encoding == constants::DW_EH_PE_omit {
        return Err(Error::CannotParseOmitPointerEncoding);
    }

    let base = match encoding.application() {
        constants::DW_EH_PE_absptr => 0,
        constants::DW_EH_PE_pcrel => input.slice().as_ptr() as u64,
        constants::DW_EH_PE_textrel => _Unwind_GetTextRelBase(unwind_ctx) as u64,
        constants::DW_EH_PE_datarel => _Unwind_GetDataRelBase(unwind_ctx) as u64,
        constants::DW_EH_PE_funcrel => _Unwind_GetRegionStart(unwind_ctx) as u64,
        constants::DW_EH_PE_aligned => return Err(Error::UnsupportedPointerEncoding),
        _ => unreachable!(),
    };

    let offset = match encoding.format() {
        constants::DW_EH_PE_absptr => input.read_address(mem::size_of::<usize>() as _),
        constants::DW_EH_PE_uleb128 => input.read_uleb128(),
        constants::DW_EH_PE_udata2 => input.read_u16().map(u64::from),
        constants::DW_EH_PE_udata4 => input.read_u32().map(u64::from),
        constants::DW_EH_PE_udata8 => input.read_u64(),
        constants::DW_EH_PE_sleb128 => input.read_sleb128().map(|a| a as u64),
        constants::DW_EH_PE_sdata2 => input.read_i16().map(|a| a as u64),
        constants::DW_EH_PE_sdata4 => input.read_i32().map(|a| a as u64),
        constants::DW_EH_PE_sdata8 => input.read_i64().map(|a| a as u64),
        _ => unreachable!(),
    }?;

    let address = base.wrapping_add(offset);
    Ok(if encoding.is_indirect() {
        Pointer::Indirect(address)
    } else {
        Pointer::Direct(address)
    })
}

fn find_eh_action(
    reader: &mut StaticSlice,
    unwind_ctx: &UnwindContext<'_>,
) -> gimli::Result<EHAction> {
    let func_start = _Unwind_GetRegionStart(unwind_ctx);
    let mut ip_before_instr = 0;
    let ip = _Unwind_GetIPInfo(unwind_ctx, &mut ip_before_instr);
    let ip = if ip_before_instr != 0 { ip } else { ip - 1 };

    let start_encoding = parse_pointer_encoding(reader)?;
    let lpad_base = if !start_encoding.is_absent() {
        unsafe { deref_pointer(parse_encoded_pointer(start_encoding, unwind_ctx, reader)?) }
    } else {
        func_start
    };

    let ttype_encoding = parse_pointer_encoding(reader)?;
    if !ttype_encoding.is_absent() {
        reader.read_uleb128()?;
    }

    let call_site_encoding = parse_pointer_encoding(reader)?;
    let call_site_table_length = reader.read_uleb128()?;
    let (mut call_site_table, mut action_table) = reader.split_at(call_site_table_length as _);

    while !call_site_table.is_empty() {
        let cs_start = unsafe {
            deref_pointer(parse_encoded_pointer(
                call_site_encoding,
                unwind_ctx,
                &mut call_site_table,
            )?)
        };
        let cs_len = unsafe {
            deref_pointer(parse_encoded_pointer(
                call_site_encoding,
                unwind_ctx,
                &mut call_site_table,
            )?)
        };
        let cs_lpad = unsafe {
            deref_pointer(parse_encoded_pointer(
                call_site_encoding,
                unwind_ctx,
                &mut call_site_table,
            )?)
        };
        let cs_action = call_site_table.read_uleb128()?;
        if ip < func_start + cs_start {
            break;
        }
        if ip < func_start + cs_start + cs_len {
            if cs_lpad == 0 {
                return Ok(EHAction::None);
            } else {
                let lpad = lpad_base + cs_lpad;
                if cs_action == 0 {
                    return Ok(EHAction::Cleanup(lpad));
                }

                action_table.skip((cs_action - 1) as _)?;
                let ttype_index = action_table.read_sleb128()?;
                return Ok(if ttype_index == 0 {
                    EHAction::Cleanup(lpad)
                } else if ttype_index > 0 {
                    EHAction::Catch(lpad)
                } else {
                    EHAction::Filter(lpad)
                });
            }
        }
    }
    Ok(EHAction::Terminate)
}

/// 读取静态内核 LSDA 的有界尾部；动态用户态 finder 保留上游接口。
///
/// # Safety
/// 非内核配置的 start 必须指向上游约定的有效 LSDA；内核范围由链接器唯一提供。
unsafe fn lsda_bytes(start: *const u8) -> Option<&'static [u8]> {
    #[cfg(feature = "kernel-percpu")]
    {
        extern "C" {
            static __gcc_except_table_start: u8;
            static __gcc_except_table_end: u8;
        }
        // SAFETY: 静态链接器提供有效的永久只读段符号，此处仅计算地址，不读取符号内容。
        let (first, end) = unsafe {
            (
                core::ptr::addr_of!(__gcc_except_table_start) as usize,
                core::ptr::addr_of!(__gcc_except_table_end) as usize,
            )
        };
        let address = start as usize;
        if address < first || end <= address {
            return None;
        }
        // SAFETY: 已确认 start 落在同一个永久只读 LSDA 输出段内，长度不越其末尾。
        Some(unsafe { core::slice::from_raw_parts(start, end - address) })
    }
    #[cfg(not(feature = "kernel-percpu"))]
    {
        // SAFETY: 此分支沿上游动态 LSDA 调用契约，由调用者保证解析可达的字节有效。
        Some(unsafe { get_unlimited_slice(start) })
    }
}

/// 用编译器生成的 LSDA 为搜索或清理阶段选择同一 Rust 调用链的 landing pad。
///
/// # Safety
/// exception 与 unwind_ctx 必须由展开器独占提供，CFI/LSDA 必须属于当前有效映像。
#[lang = "eh_personality"]
unsafe fn rust_eh_personality(
    version: c_int,
    actions: UnwindAction,
    _exception_class: u64,
    exception: *mut UnwindException,
    unwind_ctx: &mut UnwindContext<'_>,
) -> UnwindReasonCode {
    if version != 1 {
        return UnwindReasonCode::FATAL_PHASE1_ERROR;
    }

    let lsda = _Unwind_GetLanguageSpecificData(unwind_ctx);
    if lsda.is_null() {
        return UnwindReasonCode::CONTINUE_UNWIND;
    }

    // SAFETY: LSDA 来自当前 FDE；内核配置同时验证其完整静态只读段边界。
    let bytes = match unsafe { lsda_bytes(lsda as _) } {
        Some(bytes) => bytes,
        None => return UnwindReasonCode::FATAL_PHASE1_ERROR,
    };
    let mut lsda = EndianSlice::new(bytes, NativeEndian);
    let eh_action = match find_eh_action(&mut lsda, unwind_ctx) {
        Ok(v) => v,
        Err(_) => return UnwindReasonCode::FATAL_PHASE1_ERROR,
    };

    if actions.contains(UnwindAction::SEARCH_PHASE) {
        match eh_action {
            EHAction::None | EHAction::Cleanup(_) => UnwindReasonCode::CONTINUE_UNWIND,
            EHAction::Catch(_) | EHAction::Filter(_) => UnwindReasonCode::HANDLER_FOUND,
            EHAction::Terminate => UnwindReasonCode::FATAL_PHASE1_ERROR,
        }
    } else {
        match eh_action {
            EHAction::None => UnwindReasonCode::CONTINUE_UNWIND,
            // Forced unwinding hits a terminate action.
            EHAction::Filter(_) if actions.contains(UnwindAction::FORCE_UNWIND) => {
                UnwindReasonCode::CONTINUE_UNWIND
            }
            EHAction::Cleanup(lpad) | EHAction::Catch(lpad) | EHAction::Filter(lpad) => {
                _Unwind_SetGR(
                    unwind_ctx,
                    Arch::UNWIND_DATA_REG.0 .0 as _,
                    exception as usize,
                );
                _Unwind_SetGR(unwind_ctx, Arch::UNWIND_DATA_REG.1 .0 as _, 0);
                _Unwind_SetIP(unwind_ctx, lpad);
                UnwindReasonCode::INSTALL_CONTEXT
            }
            EHAction::Terminate => UnwindReasonCode::FATAL_PHASE2_ERROR,
        }
    }
}
