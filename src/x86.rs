use bitflags::bitflags;
use capstone::arch::x86::X86OperandType;
use capstone::arch::ArchOperand;
use capstone::prelude::*;
use std::io::{Cursor, Write};
use std::pin::Pin;
use std::slice;

#[cfg(windows)]
use winapi::shared::minwindef::LPVOID;
#[cfg(windows)]
use winapi::um::errhandlingapi::GetLastError;
#[cfg(windows)]
use winapi::um::memoryapi::VirtualProtect;

#[cfg(unix)]
use libc::{__errno_location, c_void, mprotect, sysconf};

use crate::err::HookError;

const MAX_INST_LEN: usize = 15;
const JMP_INST_SIZE: usize = 5;

/// The routine used in a `jmp-back hook`, which means the EIP will jump back to the
/// original position after the Routine being run.
///
/// # Arguments
///
/// * regs - The registers
/// * src_addr - The address that has been hooked
pub type JmpBackRoutine = unsafe extern "cdecl" fn(regs: *mut Registers, src_addr: usize);

/// The routine used in a `function hook`, which means the Routine will replace the
/// original FUNCTION, and the EIP will `retn` directly instead of jumping back.
/// Note that the being-hooked address must be the head of a function.
///
/// # Arguments
///
/// * regs - The registers
/// * ori_func_ptr - Original function pointer. Call it after converted to the original function type.
/// * src_addr - The address that has been hooked
///
/// Return the new return value of the replaced function.
pub type RetnRoutine =
    unsafe extern "cdecl" fn(regs: *mut Registers, ori_func_ptr: usize, src_addr: usize) -> usize;

/// The routine used in a `jmp-addr hook`, which means the EIP will jump to the specified
/// address after the Routine being run.
///
/// # Arguments
///
/// * regs - The registers
/// * ori_func_ptr - Original function pointer. Call it after converted to the original function type.
/// * src_addr - The address that has been hooked
pub type JmpToAddrRoutine =
    unsafe extern "cdecl" fn(regs: *mut Registers, ori_func_ptr: usize, src_addr: usize);

/// The routine used in a `jmp-ret hook`, which means the EIP will jump to the return
/// value of the Routine.
///
/// # Arguments
///
/// * regs - The registers
/// * ori_func_ptr - Original function pointer. Call it after converted to the original function type.
/// * src_addr - The address that has been hooked
///
/// Return the address you want to jump to.
pub type JmpToRetRoutine =
    unsafe extern "cdecl" fn(regs: *mut Registers, ori_func_ptr: usize, src_addr: usize) -> usize;

/// The hooking type.
pub enum HookType {
    /// Used in a jmp-back hook
    JmpBack(JmpBackRoutine),

    /// Used in a function hook. The first element is the mnemonic of the `retn`
    /// instruction.
    Retn(usize, RetnRoutine),

    /// Used in a jmp-addr hook. The first element is the destination address
    JmpToAddr(usize, JmpToAddrRoutine),

    /// Used in a jmp-ret hook.
    JmpToRet(JmpToRetRoutine),
}

/// The common registers.
#[repr(C)]
#[derive(Debug)]
pub struct Registers {
    /// The flags register.
    pub eflags: u32,
    /// The edi register.
    pub edi: u32,
    /// The esi register.
    pub esi: u32,
    /// The ebp register.
    pub ebp: u32,
    /// The esp register.
    pub esp: u32,
    /// The ebx register.
    pub ebx: u32,
    /// The edx register.
    pub edx: u32,
    /// The ecx register.
    pub ecx: u32,
    /// The eax register.
    pub eax: u32,
}

impl Registers {
    /// Get the value by the index from register `esp`.
    ///
    /// # Arguments
    ///
    /// * cnt - The index of the arguments.
    ///
    /// # Safety
    ///
    /// Process may crash if register `esp` does not point to a valid stack.
    pub unsafe fn get_arg(&self, cnt: usize) -> u32 {
        *((self.esp as usize + cnt * 4) as usize as *mut u32)
    }
}

/// The trait which is called before and after the modifying of the `jmp` instruction.
/// Usually is used to suspend and resume all other threads, to avoid instruction colliding.
pub trait ThreadCallback {
    /// the callback before modifying `jmp` instruction, should return true if success.
    fn pre(&self) -> bool;
    /// the callback after modifying `jmp` instruction
    fn post(&self);
}

/// Option for thread callback
pub enum CallbackOption {
    /// Valid callback
    Some(Box<dyn ThreadCallback>),
    /// No callback
    None,
}

bitflags! {
    /// Hook flags
    pub struct HookFlags:u32 {
        /// If set, will not modify the memory protection of the destination address
        const NOT_MODIFY_MEMORY_PROTECT = 0x1;
    }
}

/// The entry struct in ilhook.
/// Please read the main doc to view usage.
pub struct Hooker {
    addr: usize,
    hook_type: HookType,
    thread_cb: CallbackOption,
    flags: HookFlags,
}

#[derive(Default)]
struct Inst {
    bytes: [u8; MAX_INST_LEN],
    len: u8,
    reloc_off: u8,
    reloc_addr: u32,
}

impl Inst {
    fn new(bytes: &[u8], reloc_off: u8, reloc_addr: u32) -> Self {
        let mut s = Self {
            bytes: [0; MAX_INST_LEN],
            len: bytes.len() as u8,
            reloc_off,
            reloc_addr,
        };
        if bytes.len() > MAX_INST_LEN {
            panic!("bytes too long")
        }
        s.bytes[..bytes.len()].copy_from_slice(bytes);
        s
    }
}

#[derive(Default)]
struct OriginalCode {
    buf: [u8; MAX_INST_LEN + JMP_INST_SIZE],
    len: u8,
}

/// The hook result returned by Hooker::hook.
pub struct HookPoint {
    addr: usize,
    stub: Pin<Box<[u8]>>,
    stub_prot: u32,
    origin: OriginalCode,
    thread_cb: CallbackOption,
    flags: HookFlags,
}

#[cfg(not(target_arch = "x86"))]
fn env_lock() {
    panic!("This crate should only be used in arch x86_32!")
}
#[cfg(target_arch = "x86")]
fn env_lock() {}

impl Hooker {
    /// Create a new Hooker.
    ///
    /// # Arguments
    ///
    /// * `addr` - The being-hooked address.
    /// * `hook_type` - The hook type and callback routine.
    /// * `thread_cb` - The callbacks before and after hooking.
    /// * `flags` - Hook flags
    pub fn new(
        addr: usize,
        hook_type: HookType,
        thread_cb: CallbackOption,
        flags: HookFlags,
    ) -> Self {
        env_lock();
        Self {
            addr,
            hook_type,
            thread_cb,
            flags,
        }
    }

    /// Consumes self and execute hooking. Return the HookPoint.
    ///
    /// # Safety
    ///
    /// Process may crash (instead of panic!) if:
    ///
    /// 1. addr is not a accessible memory address.
    /// 2. addr points to an incorrect position. (At the middle of an instruction, or where after it other instructions may jump)
    /// 3. Wrong Retn-val if hook_type is `HookType::Retn`. i.e. A `cdecl` function with non-zero retn-val, or a `stdcall` function with wrong retn-val.
    /// 4. Set `NOT_MODIFY_MEMORY_PROTECT` where it should not be set.
    /// 5. hook or unhook from 2 or more threads at the same time without `HookFlags::NOT_MODIFY_MEMORY_PROTECT`. Because of memory protection colliding.
    /// 6. Other unpredictable errors.
    pub unsafe fn hook(self) -> Result<HookPoint, HookError> {
        let (moved, origin) = generate_moved_code(self.addr)?;
        let stub = generate_stub(&self, moved, origin.len)?;
        let stub_prot = modify_mem_protect(stub.as_ptr() as usize, stub.len())?;
        if !self.flags.contains(HookFlags::NOT_MODIFY_MEMORY_PROTECT) {
            let old_prot = modify_mem_protect(self.addr, JMP_INST_SIZE)?;
            let ret = modify_jmp_with_thread_cb(&self, stub.as_ptr() as usize);
            recover_mem_protect(self.addr, JMP_INST_SIZE, old_prot);
            ret?;
        } else {
            modify_jmp_with_thread_cb(&self, stub.as_ptr() as usize)?;
        }
        Ok(HookPoint {
            addr: self.addr,
            stub,
            stub_prot,
            origin,
            thread_cb: self.thread_cb,
            flags: self.flags,
        })
    }
}

impl HookPoint {
    /// Consume self and unhook the address.
    pub unsafe fn unhook(self) -> Result<(), HookError> {
        self.unhook_by_ref()
    }

    fn unhook_by_ref(&self) -> Result<(), HookError> {
        let ret: Result<(), HookError>;
        if !self.flags.contains(HookFlags::NOT_MODIFY_MEMORY_PROTECT) {
            let old_prot = modify_mem_protect(self.addr, JMP_INST_SIZE)?;
            ret = recover_jmp_with_thread_cb(&self);
            recover_mem_protect(self.addr, JMP_INST_SIZE, old_prot);
        } else {
            ret = recover_jmp_with_thread_cb(&self)
        }
        recover_mem_protect(self.stub.as_ptr() as usize, self.stub.len(), self.stub_prot);
        ret
    }
}

// When the HookPoint drops, it should unhook automatically.
impl Drop for HookPoint {
    fn drop(&mut self) {
        self.unhook_by_ref().unwrap_or_default();
    }
}

#[cfg(windows)]
fn modify_mem_protect(addr: usize, len: usize) -> Result<u32, HookError> {
    let mut old_prot: u32 = 0;
    let old_prot_ptr = &mut old_prot as *mut u32;
    // PAGE_EXECUTE_READWRITE = 0x40
    let ret = unsafe { VirtualProtect(addr as LPVOID, len, 0x40, old_prot_ptr) };
    if ret == 0 {
        Err(HookError::MemoryProtect(unsafe { GetLastError() }))
    } else {
        Ok(old_prot)
    }
}

#[cfg(unix)]
fn modify_mem_protect(addr: usize, len: usize) -> Result<u32, HookError> {
    use std::convert::TryInto;
    let page_size = unsafe { sysconf(30) }; //_SC_PAGESIZE == 30
    if len > page_size.try_into().unwrap() {
        Err(HookError::InvalidParameter)
    } else {
        //(PROT_READ | PROT_WRITE | PROT_EXEC) == 7
        let ret = unsafe {
            mprotect(
                (addr & !(page_size as usize - 1)) as *mut c_void,
                page_size as usize,
                7,
            )
        };
        if ret != 0 {
            let err = unsafe { *(__errno_location()) };
            Err(HookError::MemoryProtect(err as u32))
        } else {
            // it's too complex to get the original memory protection
            Ok(7)
        }
    }
}

#[cfg(windows)]
fn recover_mem_protect(addr: usize, len: usize, old: u32) {
    let mut old_prot: u32 = 0;
    let old_prot_ptr = &mut old_prot as *mut u32;
    unsafe { VirtualProtect(addr as LPVOID, len, old, old_prot_ptr) };
}

#[cfg(unix)]
fn recover_mem_protect(addr: usize, _: usize, old: u32) {
    let page_size = unsafe { sysconf(30) }; //_SC_PAGESIZE == 30
    unsafe {
        mprotect(
            (addr & !(page_size as usize - 1)) as *mut c_void,
            page_size as usize,
            old as i32,
        )
    };
}

#[derive(PartialEq, Debug)]
struct RelocEntry {
    off: u8,
    reloc_base_off: u8,
    dest_addr: u32,
}

fn get_jmp_dest_from_inst(detail: &ArchDetail) -> u32 {
    let ops = detail.operands();
    assert_eq!(ops.len(), 1);
    if let ArchOperand::X86Operand(op) = &ops[0] {
        if let X86OperandType::Imm(v) = op.op_type {
            v as u32
        } else {
            panic!("not jmp?")
        }
    } else {
        panic!("not jmp?")
    }
}

fn move_instruction(inst: &[u8], arch_detail: &ArchDetail) -> Inst {
    let x86 = arch_detail.x86().unwrap();
    let op1 = x86.opcode()[0];
    let op2 = x86.opcode()[1];
    match op1 {
        // short jXX
        x if (x & 0xf0) == 0x70 => Inst::new(
            &[0x0f, 0x80 | (x & 0xf), 0, 0, 0, 0],
            2,
            get_jmp_dest_from_inst(&arch_detail),
        ),
        // long jXX
        0x0f if (op2 & 0xf0) == 0x80 => Inst::new(
            &[0x0f, op2, 0, 0, 0, 0],
            2,
            get_jmp_dest_from_inst(&arch_detail),
        ),
        // loop/jecxz
        x @ 0xe0..=0xe3 => Inst::new(
            &[x, 0x02, 0xeb, 0x05, 0xe9, 0, 0, 0, 0],
            5,
            get_jmp_dest_from_inst(&arch_detail),
        ),
        // short and long jmp
        0xeb | 0xe9 => Inst::new(&[0xe9, 0, 0, 0, 0], 1, get_jmp_dest_from_inst(&arch_detail)),
        // call
        0xe8 => Inst::new(&[0xe8, 0, 0, 0, 0], 1, get_jmp_dest_from_inst(&arch_detail)),
        _ => Inst::new(inst, 0, 0),
    }
}

fn generate_moved_code(addr: usize) -> Result<(Vec<Inst>, OriginalCode), HookError> {
    let cs = Capstone::new()
        .x86()
        .mode(arch::x86::ArchMode::Mode32)
        .syntax(arch::x86::ArchSyntax::Intel)
        .detail(true)
        .build()
        .expect("Failed to create Capstone object");

    let mut ret: Vec<Inst> = vec![];
    let code_slice =
        unsafe { slice::from_raw_parts(addr as *const u8, MAX_INST_LEN * JMP_INST_SIZE) };
    let mut code_idx = 0;
    while code_idx < JMP_INST_SIZE {
        let insts = match cs.disasm_count(&code_slice[code_idx..], (addr + code_idx) as u64, 1) {
            Ok(i) => i,
            Err(_) => return Err(HookError::Disassemble),
        };
        let inst = insts.iter().nth(0).unwrap();
        let detail = cs.insn_detail(&inst).unwrap();
        ret.push(move_instruction(inst.bytes(), &detail.arch_detail()));
        code_idx += inst.bytes().len();
    }
    let mut origin: OriginalCode = Default::default();
    origin.len = code_idx as u8;
    origin.buf[..code_idx].copy_from_slice(&code_slice[..code_idx]);
    Ok((ret, origin))
}

fn write_moved_code_to_buf(
    code: &Vec<Inst>,
    buf: &mut Cursor<Vec<u8>>,
    reloc_tbl: &mut Vec<RelocEntry>,
) {
    code.iter().for_each(|inst| {
        if inst.reloc_off != 0 {
            reloc_tbl.push(RelocEntry {
                off: buf.position() as u8 + inst.reloc_off,
                reloc_base_off: 4,
                dest_addr: inst.reloc_addr,
            });
        }
        buf.write(&inst.bytes[..inst.len as usize]).unwrap();
    });
}

fn jmp_addr(
    addr: u32,
    buf: &mut Cursor<Vec<u8>>,
    rel_tbl: &mut Vec<RelocEntry>,
) -> Result<(), HookError> {
    buf.write(&[0xe9])?;
    rel_tbl.push(RelocEntry {
        off: buf.position() as u8,
        reloc_base_off: 4,
        dest_addr: addr,
    });
    buf.write(&[0, 0, 0, 0])?;
    Ok(())
}

fn relocate_addr(
    buf: Pin<&mut [u8]>,
    rel_tbl: Vec<RelocEntry>,
    addr_to_write: u8,
    moved_code_off: u8,
) {
    let buf = unsafe { Pin::get_unchecked_mut(buf) };
    let buf_addr = buf.as_ptr() as usize as u32;
    rel_tbl.iter().for_each(|ent| {
        let off = ent.off as usize;
        let relative_addr =
            ent.dest_addr as i32 - (buf_addr as i32 + off as i32 + ent.reloc_base_off as i32);
        buf[off..off + 4].copy_from_slice(&relative_addr.to_le_bytes());
    });

    if addr_to_write != 0 {
        let addr_to_write = addr_to_write as usize;
        buf[addr_to_write..addr_to_write + 4]
            .copy_from_slice(&(buf_addr + moved_code_off as u32).to_le_bytes());
    }
}

fn generate_jmp_back_stub(
    buf: &mut Cursor<Vec<u8>>,
    rel_tbl: &mut Vec<RelocEntry>,
    moved_code: Vec<Inst>,
    ori_addr: usize,
    cb: JmpBackRoutine,
    ori_len: u8,
) -> Result<(u8, u8), HookError> {
    // push hooker.addr
    buf.write(&[0x68])?;
    buf.write(&(ori_addr as u32).to_le_bytes())?;

    // push ebp (Registers)
    // call XXXX (dest addr)
    buf.write(&[0x55, 0xe8])?;
    rel_tbl.push(RelocEntry {
        off: buf.position() as u8,
        dest_addr: cb as usize as u32,
        reloc_base_off: 4,
    });

    buf.write(&(cb as usize as u32).to_le_bytes())?;
    // add esp, 0x8
    buf.write(&[0x83, 0xc4, 0x08])?;
    // popfd
    // popad
    buf.write(&[0x9d, 0x61])?;
    write_moved_code_to_buf(&moved_code, buf, rel_tbl);
    // jmp back
    jmp_addr(ori_addr as u32 + ori_len as u32, buf, rel_tbl)?;
    Ok((0, 0))
}

fn generate_retn_stub(
    buf: &mut Cursor<Vec<u8>>,
    rel_tbl: &mut Vec<RelocEntry>,
    moved_code: Vec<Inst>,
    ori_addr: usize,
    retn_val: u16,
    cb: RetnRoutine,
    ori_len: u8,
) -> Result<(u8, u8), HookError> {
    // push hooker.addr
    buf.write(&[0x68])?;
    buf.write(&(ori_addr as u32).to_le_bytes())?;

    // push XXXX (original function addr)
    // push ebp (Registers)
    // call XXXX (dest addr)
    let ori_func_addr_off = buf.position() as u8 + 1;
    buf.write(&[0x68, 0, 0, 0, 0, 0x55, 0xe8])?;
    rel_tbl.push(RelocEntry {
        off: buf.position() as u8,
        reloc_base_off: 4,
        dest_addr: cb as usize as u32,
    });

    buf.write(&[0, 0, 0, 0])?;
    // add esp, 0xc
    buf.write(&[0x83, 0xc4, 0x0c])?;
    // mov [esp+20h], eax
    buf.write(&[0x89, 0x44, 0x24, 0x20])?;
    // popfd
    // popad
    buf.write(&[0x9d, 0x61])?;
    if retn_val == 0 {
        // retn
        buf.write(&[0xc3])?;
    } else {
        // retn XX
        buf.write(&[0xc2])?;
        buf.write(&retn_val.to_le_bytes())?;
    }
    let ori_func_off = buf.position() as u8;
    write_moved_code_to_buf(&moved_code, buf, rel_tbl);
    // jmp ori_addr
    jmp_addr(ori_addr as u32 + ori_len as u32, buf, rel_tbl)?;
    Ok((ori_func_addr_off, ori_func_off))
}

fn generate_jmp_addr_stub(
    buf: &mut Cursor<Vec<u8>>,
    rel_tbl: &mut Vec<RelocEntry>,
    moved_code: Vec<Inst>,
    ori_addr: usize,
    dest_addr: usize,
    cb: JmpToAddrRoutine,
    ori_len: u8,
) -> Result<(u8, u8), HookError> {
    // push hooker.addr
    buf.write(&[0x68])?;
    buf.write(&(ori_addr as u32).to_le_bytes())?;

    // push XXXX (original function addr)
    // push ebp (Registers)
    // call XXXX (dest addr)
    let ori_func_addr_off = buf.position() as u8 + 1;
    buf.write(&[0x68, 0, 0, 0, 0, 0x55, 0xe8])?;
    rel_tbl.push(RelocEntry {
        off: buf.position() as u8,
        dest_addr: cb as usize as u32,
        reloc_base_off: 4,
    });

    buf.write(&[0, 0, 0, 0])?;
    // add esp, 0xc
    buf.write(&[0x83, 0xc4, 0x0c])?;
    // popfd
    // popad
    buf.write(&[0x9d, 0x61])?;
    // jmp back
    jmp_addr(dest_addr as u32 + ori_len as u32, buf, rel_tbl)?;

    let ori_func_off = buf.position() as u8;
    write_moved_code_to_buf(&moved_code, buf, rel_tbl);
    // jmp ori_addr
    jmp_addr(ori_addr as u32 + ori_len as u32, buf, rel_tbl)?;
    Ok((ori_func_addr_off, ori_func_off))
}

fn generate_jmp_ret_stub(
    buf: &mut Cursor<Vec<u8>>,
    rel_tbl: &mut Vec<RelocEntry>,
    moved_code: Vec<Inst>,
    ori_addr: usize,
    cb: JmpToRetRoutine,
    ori_len: u8,
) -> Result<(u8, u8), HookError> {
    // push hooker.addr
    buf.write(&[0x68])?;
    buf.write(&(ori_addr as u32).to_le_bytes())?;

    // push XXXX (original function addr)
    // push ebp (Registers)
    // call XXXX (dest addr)
    let ori_func_addr_off = buf.position() as u8 + 1;
    buf.write(&[0x68, 0, 0, 0, 0, 0x55, 0xe8])?;
    rel_tbl.push(RelocEntry {
        off: buf.position() as u8,
        reloc_base_off: 4,
        dest_addr: cb as usize as u32,
    });

    buf.write(&[0, 0, 0, 0])?;
    // add esp, 0xc
    buf.write(&[0x83, 0xc4, 0x0c])?;
    // mov [esp-4], eax
    buf.write(&[0x89, 0x44, 0x24, 0xfc])?;
    // popfd
    // popad
    buf.write(&[0x9d, 0x61])?;
    // jmp dword ptr [esp-0x28]
    buf.write(&[0xff, 0x64, 0x24, 0xd8])?;

    let ori_func_off = buf.position() as u8;
    write_moved_code_to_buf(&moved_code, buf, rel_tbl);
    // jmp dest_addr
    jmp_addr(ori_addr as u32 + ori_len as u32, buf, rel_tbl)?;
    Ok((ori_func_addr_off, ori_func_off))
}

fn generate_stub(
    hooker: &Hooker,
    moved_code: Vec<Inst>,
    ori_len: u8,
) -> Result<Pin<Box<[u8]>>, HookError> {
    let mut rel_tbl = Vec::<RelocEntry>::new();
    let mut buf = Cursor::new(Vec::<u8>::with_capacity(100));
    // pushad
    // pushfd
    // mov ebp, esp
    buf.write(&[0x60, 0x9c, 0x8b, 0xec])?;

    let (ori_func_addr_off, ori_func_off) = match hooker.hook_type {
        HookType::JmpBack(cb) => {
            generate_jmp_back_stub(&mut buf, &mut rel_tbl, moved_code, hooker.addr, cb, ori_len)
        }
        HookType::Retn(val, cb) => generate_retn_stub(
            &mut buf,
            &mut rel_tbl,
            moved_code,
            hooker.addr,
            val as u16,
            cb,
            ori_len,
        ),
        HookType::JmpToAddr(dest, cb) => generate_jmp_addr_stub(
            &mut buf,
            &mut rel_tbl,
            moved_code,
            hooker.addr,
            dest,
            cb,
            ori_len,
        ),
        HookType::JmpToRet(cb) => {
            generate_jmp_ret_stub(&mut buf, &mut rel_tbl, moved_code, hooker.addr, cb, ori_len)
        }
    }?;

    let mut p = Pin::new(buf.into_inner().into_boxed_slice());
    relocate_addr(p.as_mut(), rel_tbl, ori_func_addr_off, ori_func_off);
    Ok(p)
}

fn modify_jmp(dest_addr: usize, stub_addr: usize) -> Result<(), HookError> {
    let buf = unsafe { slice::from_raw_parts_mut(dest_addr as *mut u8, JMP_INST_SIZE) };
    // jmp stub_addr
    buf[0] = 0xe9;
    let rel_off = stub_addr as i32 - (dest_addr as i32 + 5);
    buf[1..5].copy_from_slice(&rel_off.to_le_bytes());
    Ok(())
}

fn modify_jmp_with_thread_cb(hook: &Hooker, stub_addr: usize) -> Result<(), HookError> {
    if let CallbackOption::Some(cbs) = &hook.thread_cb {
        if !cbs.pre() {
            return Err(HookError::PreHook);
        }
        let ret = modify_jmp(hook.addr, stub_addr);
        cbs.post();
        ret
    } else {
        modify_jmp(hook.addr, stub_addr)
    }
}

fn recover_jmp(dest_addr: usize, origin: &[u8]) {
    let buf = unsafe { slice::from_raw_parts_mut(dest_addr as *mut u8, origin.len()) };
    // jmp stub_addr
    buf.copy_from_slice(origin);
}

fn recover_jmp_with_thread_cb(hook: &HookPoint) -> Result<(), HookError> {
    if let CallbackOption::Some(cbs) = &hook.thread_cb {
        if !cbs.pre() {
            return Err(HookError::PreHook);
        }
        recover_jmp(hook.addr, &hook.origin.buf[..hook.origin.len as usize]);
        cbs.post();
    } else {
        recover_jmp(hook.addr, &hook.origin.buf[..hook.origin.len as usize]);
    }
    Ok(())
}

#[cfg(target_arch = "x86")]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn test_move_inst_1() {
        let cs = Capstone::new()
            .x86()
            .mode(arch::x86::ArchMode::Mode64)
            .syntax(arch::x86::ArchSyntax::Intel)
            .detail(true)
            .build()
            .expect("Failed to create Capstone object");

        // jmp eax
        let inst = [0xff, 0xe0];
        let insts = cs.disasm_count(&inst, inst.as_ptr() as u64, 1).unwrap();
        let inst_info = insts.iter().nth(0).unwrap();
        let insn_detail = cs.insn_detail(&inst_info).unwrap();
        let arch_detail = insn_detail.arch_detail();
        let new_inst = move_instruction(&inst, &arch_detail);
        assert_eq!(new_inst.bytes[..2], inst);
        assert_eq!(new_inst.len, 2);
        assert_eq!(new_inst.reloc_off, 0);
    }
    #[test]
    fn test_move_inst_2() {
        let cs = Capstone::new()
            .x86()
            .mode(arch::x86::ArchMode::Mode64)
            .syntax(arch::x86::ArchSyntax::Intel)
            .detail(true)
            .build()
            .expect("Failed to create Capstone object");
        // jmp @-2
        let inst = [0xeb, 0xfe];
        let insts = cs.disasm_count(&inst, inst.as_ptr() as u64, 1).unwrap();
        let inst_info = insts.iter().nth(0).unwrap();
        let insn_detail = cs.insn_detail(&inst_info).unwrap();
        let arch_detail = insn_detail.arch_detail();
        let new_inst = move_instruction(&inst, &arch_detail);
        let addr = inst.as_ptr() as usize as i32;
        assert_eq!(new_inst.bytes[0], 0xe9);
        assert_eq!(new_inst.reloc_addr, (addr + 2 - 2) as u32);
        assert_eq!(new_inst.len, 5);
        assert_eq!(new_inst.reloc_off, 1);
    }
    #[test]
    fn test_move_inst_3() {
        let cs = Capstone::new()
            .x86()
            .mode(arch::x86::ArchMode::Mode64)
            .syntax(arch::x86::ArchSyntax::Intel)
            .detail(true)
            .build()
            .expect("Failed to create Capstone object");
        // jmp @-0x20
        let inst = [0xe9, 0xe0, 0xff, 0xff, 0xff];
        let insts = cs.disasm_count(&inst, inst.as_ptr() as u64, 1).unwrap();
        let inst_info = insts.iter().nth(0).unwrap();
        let insn_detail = cs.insn_detail(&inst_info).unwrap();
        let arch_detail = insn_detail.arch_detail();
        let new_inst = move_instruction(&inst, &arch_detail);
        let addr = inst.as_ptr() as usize as i32;
        assert_eq!(new_inst.bytes[0], 0xe9);
        assert_eq!(new_inst.reloc_addr, (addr + 5 - 0x20) as u32);
        assert_eq!(new_inst.len, 5);
        assert_eq!(new_inst.reloc_off, 1);
    }
    #[test]
    fn test_move_inst_4() {
        let cs = Capstone::new()
            .x86()
            .mode(arch::x86::ArchMode::Mode64)
            .syntax(arch::x86::ArchSyntax::Intel)
            .detail(true)
            .build()
            .expect("Failed to create Capstone object");
        // call @+10
        let inst = [0xe8, 0xa, 0, 0, 0];
        let insts = cs.disasm_count(&inst, inst.as_ptr() as u64, 1).unwrap();
        let inst_info = insts.iter().nth(0).unwrap();
        let insn_detail = cs.insn_detail(&inst_info).unwrap();
        let arch_detail = insn_detail.arch_detail();
        let new_inst = move_instruction(&inst, &arch_detail);
        let addr = inst.as_ptr() as usize as i32;
        assert_eq!(new_inst.bytes[0], 0xe8);
        assert_eq!(new_inst.reloc_addr, (addr + 5 + 10) as u32);
        assert_eq!(new_inst.len, 5);
        assert_eq!(new_inst.reloc_off, 1);
    }
    #[test]
    fn test_move_inst_5() {
        let cs = Capstone::new()
            .x86()
            .mode(arch::x86::ArchMode::Mode64)
            .syntax(arch::x86::ArchSyntax::Intel)
            .detail(true)
            .build()
            .expect("Failed to create Capstone object");
        // jnz @+0
        let inst = [0x75, 0];
        let insts = cs.disasm_count(&inst, inst.as_ptr() as u64, 1).unwrap();
        let inst_info = insts.iter().nth(0).unwrap();
        let insn_detail = cs.insn_detail(&inst_info).unwrap();
        let arch_detail = insn_detail.arch_detail();
        let new_inst = move_instruction(&inst, &arch_detail);
        let addr = inst.as_ptr() as usize as i32;
        assert_eq!(new_inst.bytes[0..2], [0x0f, 0x85]);
        assert_eq!(new_inst.reloc_addr, (addr + 2) as u32);
        assert_eq!(new_inst.len, 6);
        assert_eq!(new_inst.reloc_off, 2);
    }
    #[test]
    fn test_move_inst_6() {
        let cs = Capstone::new()
            .x86()
            .mode(arch::x86::ArchMode::Mode64)
            .syntax(arch::x86::ArchSyntax::Intel)
            .detail(true)
            .build()
            .expect("Failed to create Capstone object");
        // jnz @-6
        let inst = [0x0f, 0x85, 0xfa, 0xff, 0xff, 0xff];
        let insts = cs.disasm_count(&inst, inst.as_ptr() as u64, 1).unwrap();
        let inst_info = insts.iter().nth(0).unwrap();
        let insn_detail = cs.insn_detail(&inst_info).unwrap();
        let arch_detail = insn_detail.arch_detail();
        let new_inst = move_instruction(&inst, &arch_detail);
        let addr = inst.as_ptr() as usize as i32;
        assert_eq!(new_inst.bytes[0..2], [0x0f, 0x85]);
        assert_eq!(new_inst.reloc_addr, addr as u32);
        assert_eq!(new_inst.len, 6);
        assert_eq!(new_inst.reloc_off, 2);
    }
    #[test]
    fn test_move_inst_7() {
        let cs = Capstone::new()
            .x86()
            .mode(arch::x86::ArchMode::Mode64)
            .syntax(arch::x86::ArchSyntax::Intel)
            .detail(true)
            .build()
            .expect("Failed to create Capstone object");
        // jecxz @+10
        let inst = [0xe3, 0x02];
        let insts = cs.disasm_count(&inst, inst.as_ptr() as u64, 1).unwrap();
        let inst_info = insts.iter().nth(0).unwrap();
        let insn_detail = cs.insn_detail(&inst_info).unwrap();
        let arch_detail = insn_detail.arch_detail();
        let new_inst = move_instruction(&inst, &arch_detail);
        let addr = inst.as_ptr() as usize as i32;
        //assert_eq!(new_inst.bytes[0..5], [0xe3, 0x02, 0xeb, 0x05, 0xe9]);
        assert_eq!(new_inst.reloc_addr, (addr + 4) as u32);
        assert_eq!(new_inst.len, 9);
        assert_eq!(new_inst.reloc_off, 5);
    }
    #[test]
    fn test_relocate_addr() {
        let b: Box<[u8]> =
            vec![3, 0, 0, 0, 0, 0xf7, 0xff, 0xff, 0xff, 2, 0, 0, 0].into_boxed_slice();
        let mut p = Pin::new(b);
        let addr = p.as_ptr() as usize as i32;
        let off1 = [0, 0, 0, 0];
        let off2 = (addr + 9 - 9).to_le_bytes();
        let off3 = (addr + 13 + 2).to_le_bytes();
        let x: Vec<u8> = [3]
            .iter()
            .chain(off1.iter())
            .chain(off2.iter())
            .chain(off3.iter())
            .cloned()
            .collect();
        p.copy_from_slice(&x[..]);
        let rel_tbl: Vec<RelocEntry> = vec![
            RelocEntry {
                off: 5,
                reloc_base_off: 4,
                dest_addr: addr as u32,
            },
            RelocEntry {
                off: 9,
                reloc_base_off: 4,
                dest_addr: (addr + 13 + 2) as u32,
            },
        ];
        relocate_addr(p.as_mut(), rel_tbl, 1, 9);
        let off1 = (addr + 9).to_le_bytes();
        let off2 = [0xf7, 0xff, 0xff, 0xff];
        let off3 = [2, 0, 0, 0];
        let b: Vec<&u8> = [3]
            .iter()
            .chain(off1.iter())
            .chain(off2.iter())
            .chain(off3.iter())
            .collect();
        assert_eq!(p.iter().cmp(b.iter().cloned()), std::cmp::Ordering::Equal);
    }
    #[test]
    fn test_generate_code() {
        let b: Box<[u8]> = vec![0x53, 0x83, 0xec, 0x18, 0xe8, 0, 0, 0, 0, 0x58].into_boxed_slice();
        let p = Pin::new(b);
        let addr = p.as_ptr() as usize;
        let (moved_code, origin) = generate_moved_code(addr).unwrap();
        assert_eq!(moved_code.len(), 3);
        assert_eq!(moved_code[0].len, 1);
        assert_eq!(moved_code[1].len, 3);
        assert_eq!(moved_code[2].len, 5);
        assert_eq!(moved_code[2].bytes[0], 0xe8);
        //assert_eq!(moved_code[2].bytes[1..5], (addr as u32 + 9).to_le_bytes());
        assert_eq!(moved_code[2].reloc_addr, addr as u32 + 9);
        assert_eq!(moved_code[2].reloc_off, 1);
        assert_eq!(origin.len, 9);
        assert_eq!(origin.buf[..9], [0x53, 0x83, 0xec, 0x18, 0xe8, 0, 0, 0, 0]);
    }
    #[test]
    fn test_write_moved_code() {
        let b: Box<[u8]> = vec![0x53, 0x83, 0xec, 0x18, 0xe8, 0, 0, 0, 0, 0x58].into_boxed_slice();
        let p = Pin::new(b);
        let addr = p.as_ptr() as usize;
        let (moved_code, _) = generate_moved_code(addr).unwrap();
        let mut rel_tbl = Vec::<RelocEntry>::new();
        let mut buf = Cursor::new(Vec::<u8>::with_capacity(100));
        write_moved_code_to_buf(&moved_code, &mut buf, &mut rel_tbl);
        assert_eq!(buf.into_inner(), [0x53, 0x83, 0xec, 0x18, 0xe8, 0, 0, 0, 0]);
        assert_eq!(
            rel_tbl,
            [RelocEntry {
                off: 5,
                reloc_base_off: 4,
                dest_addr: (addr + 9) as u32
            }]
        );
    }

    #[cfg(test)]
    fn foo(x: u32) -> u32 {
        x * x
    }
    #[cfg(test)]
    unsafe extern "cdecl" fn on_foo(reg: *mut Registers, old_func: usize, _: usize) -> usize {
        let old_func = std::mem::transmute::<usize, fn(u32) -> u32>(old_func);
        old_func((*reg).get_arg(1)) as usize + 3
    }

    #[test]
    fn test_hook_function_cdecl() {
        assert_eq!(foo(5), 25);
        let hooker = Hooker::new(
            foo as usize,
            HookType::Retn(0, on_foo),
            CallbackOption::None,
            HookFlags::empty(),
        );
        let info = unsafe { hooker.hook().unwrap() };
        assert_eq!(foo(5), 28);
        unsafe { info.unhook().unwrap() };
        assert_eq!(foo(5), 25);
    }

    #[cfg(test)]
    extern "stdcall" fn foo2(x: u32) -> u32 {
        x * x
    }
    #[cfg(test)]
    unsafe extern "cdecl" fn on_foo2(reg: *mut Registers, old_func: usize, _: usize) -> usize {
        let old_func = std::mem::transmute::<usize, extern "stdcall" fn(u32) -> u32>(old_func);
        old_func((*reg).get_arg(1)) as usize + 3
    }
    #[test]
    fn test_hook_function_stdcall() {
        assert_eq!(foo2(5), 25);
        let hooker = Hooker::new(
            foo2 as usize,
            HookType::Retn(4, on_foo2),
            CallbackOption::None,
            HookFlags::empty(),
        );
        let info = unsafe { hooker.hook().unwrap() };
        assert_eq!(foo2(5), 28);
        unsafe { info.unhook().unwrap() };
        assert_eq!(foo2(5), 25);
    }
}
