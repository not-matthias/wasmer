use crate::arm64_decl::new_machine_state;
use crate::arm64_decl::{ARM64Register, ArgumentRegisterAllocator, GPR, NEON};
use crate::common_decl::*;
use crate::emitter_arm64::*;
use crate::location::Location as AbstractLocation;
use crate::machine::*;
use dynasm::dynasm;
use dynasmrt::{aarch64::Aarch64Relocation, VecAssembler};
use std::collections::HashSet;
use wasmer_compiler::wasmparser::Type as WpType;
use wasmer_compiler::{
    CallingConvention, CustomSection, CustomSectionProtection, FunctionBody, InstructionAddressMap,
    Relocation, RelocationKind, RelocationTarget, SectionBody, SourceLoc, TrapInformation,
};
use wasmer_types::{FunctionIndex, FunctionType, Type};
use wasmer_vm::{TrapCode, VMOffsets};

type Assembler = VecAssembler<Aarch64Relocation>;
type Location = AbstractLocation<GPR, NEON>;

pub struct MachineARM64 {
    assembler: Assembler,
    used_gprs: HashSet<GPR>,
    used_simd: HashSet<NEON>,
    trap_table: TrapTable,
    /// Map from byte offset into wasm function to range of native instructions.
    ///
    // Ordered by increasing InstructionAddressMap::srcloc.
    instructions_address_map: Vec<InstructionAddressMap>,
    /// The source location for the current operator.
    src_loc: u32,
    /// is last push on a 8byte multiple or 16bytes?
    pushed: bool,
}

impl MachineARM64 {
    pub fn new() -> Self {
        MachineARM64 {
            assembler: Assembler::new(0),
            used_gprs: HashSet::new(),
            used_simd: HashSet::new(),
            trap_table: TrapTable::default(),
            instructions_address_map: vec![],
            src_loc: 0,
            pushed: false,
        }
    }
    fn emit_relaxed_binop(
        &mut self,
        op: fn(&mut Assembler, Size, Location, Location),
        sz: Size,
        src: Location,
        dst: Location,
    ) {
        match (src, dst) {
            (Location::GPR(_), Location::GPR(_)) => {
                op(&mut self.assembler, sz, src, dst);
            }
            (Location::Memory(_, _), Location::Memory(_, _)) => {
                let temp_src = self.acquire_temp_gpr().unwrap();
                let temp_dst = self.acquire_temp_gpr().unwrap();
                self.move_location(sz, src, Location::GPR(temp_src));
                self.move_location(sz, dst, Location::GPR(temp_dst));
                op(
                    &mut self.assembler,
                    sz,
                    Location::GPR(temp_src),
                    Location::GPR(temp_dst),
                );
                self.release_gpr(temp_dst);
                self.release_gpr(temp_src);
            }
            /*(Location::Imm64(_), Location::Imm64(_)) | (Location::Imm64(_), Location::Imm32(_)) => {
            }
            (_, Location::Imm32(_)) | (_, Location::Imm64(_)) => RelaxMode::DstToGPR,*/
            (Location::Imm64(_), Location::Memory(_, _))
            | (Location::Imm64(_), Location::GPR(_))
            | (Location::Imm32(_), Location::Memory(_, _))
            | (Location::Imm32(_), Location::GPR(_)) => {
                let temp = self.acquire_temp_gpr().unwrap();
                self.move_location(sz, src, Location::GPR(temp));
                op(&mut self.assembler, sz, Location::GPR(temp), dst);
                self.release_gpr(temp);
            }
            (_, Location::SIMD(_)) => {
                let temp = self.acquire_temp_gpr().unwrap();
                self.move_location(sz, src, Location::GPR(temp));
                op(&mut self.assembler, sz, Location::GPR(temp), dst);
                self.release_gpr(temp);
            }
            _ => panic!(
                "singlepass can't emit relaxed_binop {:?} {:?} => {:?}",
                sz, src, dst
            ),
        };
    }
    /// I32 binary operation with both operands popped from the virtual stack.
    fn emit_binop_i32(
        &mut self,
        f: fn(&mut Assembler, Size, Location, Location),
        loc_a: Location,
        loc_b: Location,
        ret: Location,
    ) {
        if loc_a != ret {
            let tmp = self.acquire_temp_gpr().unwrap();
            self.emit_relaxed_mov(Size::S32, loc_a, Location::GPR(tmp));
            self.emit_relaxed_binop(f, Size::S32, loc_b, Location::GPR(tmp));
            self.emit_relaxed_mov(Size::S32, Location::GPR(tmp), ret);
            self.release_gpr(tmp);
        } else {
            self.emit_relaxed_binop(f, Size::S32, loc_b, ret);
        }
    }
    /// I64 binary operation with both operands popped from the virtual stack.
    fn emit_binop_i64(
        &mut self,
        f: fn(&mut Assembler, Size, Location, Location),
        loc_a: Location,
        loc_b: Location,
        ret: Location,
    ) {
        if loc_a != ret {
            let tmp = self.acquire_temp_gpr().unwrap();
            self.emit_relaxed_mov(Size::S64, loc_a, Location::GPR(tmp));
            self.emit_relaxed_binop(f, Size::S64, loc_b, Location::GPR(tmp));
            self.emit_relaxed_mov(Size::S64, Location::GPR(tmp), ret);
            self.release_gpr(tmp);
        } else {
            self.emit_relaxed_binop(f, Size::S64, loc_b, ret);
        }
    }
    /// I64 comparison with.
    fn emit_cmpop_i64_dynamic_b(
        &mut self,
        _c: Condition,
        _loc_a: Location,
        _loc_b: Location,
        _ret: Location,
    ) {
        unimplemented!();
    }
    /// I64 shift with both operands popped from the virtual stack.
    fn emit_shift_i64(
        &mut self,
        _f: fn(&mut Assembler, Size, Location, Location),
        _loc_a: Location,
        _loc_b: Location,
        _ret: Location,
    ) {
        unimplemented!();
    }
    /// I32 comparison with.
    fn emit_cmpop_i32_dynamic_b(
        &mut self,
        _c: Condition,
        _loc_a: Location,
        _loc_b: Location,
        _ret: Location,
    ) {
        unimplemented!();
    }
    /// I32 shift with both operands popped from the virtual stack.
    fn emit_shift_i32(
        &mut self,
        _f: fn(&mut Assembler, Size, Location, Location),
        _loc_a: Location,
        _loc_b: Location,
        _ret: Location,
    ) {
        unimplemented!();
    }

    fn memory_op<F: FnOnce(&mut Self, GPR)>(
        &mut self,
        addr: Location,
        memarg: &MemoryImmediate,
        check_alignment: bool,
        value_size: usize,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
        cb: F,
    ) {
        let tmp_addr = self.acquire_temp_gpr().unwrap();

        // Reusing `tmp_addr` for temporary indirection here, since it's not used before the last reference to `{base,bound}_loc`.
        let (base_loc, bound_loc) = if imported_memories {
            // Imported memories require one level of indirection.
            self.emit_relaxed_binop(
                Assembler::emit_mov,
                Size::S64,
                Location::Memory(self.get_vmctx_reg(), offset),
                Location::GPR(tmp_addr),
            );
            (Location::Memory(tmp_addr, 0), Location::Memory(tmp_addr, 8))
        } else {
            (
                Location::Memory(self.get_vmctx_reg(), offset),
                Location::Memory(self.get_vmctx_reg(), offset + 8),
            )
        };

        let tmp_base = self.acquire_temp_gpr().unwrap();
        let tmp_bound = self.acquire_temp_gpr().unwrap();

        // Load base into temporary register.
        self.assembler
            .emit_ldr(Size::S64, Location::GPR(tmp_base), base_loc);

        // Load bound into temporary register, if needed.
        if need_check {
            self.assembler
                .emit_ldr(Size::S64, Location::GPR(tmp_bound), bound_loc);

            // Wasm -> Effective.
            // Assuming we never underflow - should always be true on Linux/macOS and Windows >=8,
            // since the first page from 0x0 to 0x1000 is not accepted by mmap.
            self.assembler.emit_add(
                Size::S64,
                Location::GPR(tmp_bound),
                Location::GPR(tmp_base),
                Location::GPR(tmp_bound),
            );
            if value_size < 256 {
                self.assembler.emit_sub(
                    Size::S64,
                    Location::GPR(tmp_bound),
                    Location::GPR(tmp_bound),
                    Location::Imm8(value_size as u8),
                );
            } else {
                // reusing tmp_base
                self.assembler
                    .emit_mov_imm(Location::GPR(tmp_base), value_size as u64);
                self.assembler.emit_sub(
                    Size::S64,
                    Location::GPR(tmp_bound),
                    Location::GPR(tmp_base),
                    Location::GPR(tmp_bound),
                );
            }
        }

        // Load effective address.
        // `base_loc` and `bound_loc` becomes INVALID after this line, because `tmp_addr`
        // might be reused.
        self.assembler
            .emit_mov(Size::S32, addr, Location::GPR(tmp_addr));

        // Add offset to memory address.
        if memarg.offset != 0 {
            self.assembler.emit_add(
                Size::S32,
                Location::Imm32(memarg.offset),
                Location::GPR(tmp_addr),
                Location::GPR(tmp_addr),
            );

            // Trap if offset calculation overflowed.
            self.assembler
                .emit_bcond_label(Condition::Cs, heap_access_oob);
        }

        // Wasm linear memory -> real memory
        self.assembler.emit_add(
            Size::S64,
            Location::GPR(tmp_base),
            Location::GPR(tmp_addr),
            Location::GPR(tmp_addr),
        );

        if need_check {
            // Trap if the end address of the requested area is above that of the linear memory.
            self.assembler
                .emit_cmp(Size::S64, Location::GPR(tmp_bound), Location::GPR(tmp_addr));

            // `tmp_bound` is inclusive. So trap only if `tmp_addr > tmp_bound`.
            self.assembler
                .emit_bcond_label(Condition::Hi, heap_access_oob);
        }

        self.release_gpr(tmp_bound);
        self.release_gpr(tmp_base);

        let align = memarg.align;
        if check_alignment && align != 1 {
            self.assembler.emit_tst(
                Size::S64,
                Location::Imm32((align - 1).into()),
                Location::GPR(tmp_addr),
            );
            self.assembler
                .emit_bcond_label(Condition::Ne, heap_access_oob);
        }
        let begin = self.assembler.get_offset().0;
        cb(self, tmp_addr);
        let end = self.assembler.get_offset().0;
        self.mark_address_range_with_trap_code(TrapCode::HeapAccessOutOfBounds, begin, end);

        self.release_gpr(tmp_addr);
    }

    fn emit_compare_and_swap<F: FnOnce(&mut Self, GPR, GPR)>(
        &mut self,
        _loc: Location,
        _target: Location,
        _ret: Location,
        _memarg: &MemoryImmediate,
        _value_size: usize,
        _memory_sz: Size,
        _stack_sz: Size,
        _need_check: bool,
        _imported_memories: bool,
        _offset: i32,
        _heap_access_oob: Label,
        _cb: F,
    ) {
        unimplemented!();
    }

    // Checks for underflow/overflow/nan.
    fn emit_f32_int_conv_check(
        &mut self,
        _reg: NEON,
        _lower_bound: f32,
        _upper_bound: f32,
        _underflow_label: Label,
        _overflow_label: Label,
        _nan_label: Label,
        _succeed_label: Label,
    ) {
        unimplemented!();
    }

    // Checks for underflow/overflow/nan before IxxTrunc{U/S}F32.
    fn emit_f32_int_conv_check_trap(&mut self, reg: NEON, lower_bound: f32, upper_bound: f32) {
        unimplemented!();
    }
    fn emit_f32_int_conv_check_sat<
        F1: FnOnce(&mut Self),
        F2: FnOnce(&mut Self),
        F3: FnOnce(&mut Self),
        F4: FnOnce(&mut Self),
    >(
        &mut self,
        _reg: NEON,
        _lower_bound: f32,
        _upper_bound: f32,
        _underflow_cb: F1,
        _overflow_cb: F2,
        _nan_cb: Option<F3>,
        _convert_cb: F4,
    ) {
        unimplemented!();
    }
    // Checks for underflow/overflow/nan.
    fn emit_f64_int_conv_check(
        &mut self,
        _reg: NEON,
        _lower_bound: f64,
        _upper_bound: f64,
        _underflow_label: Label,
        _overflow_label: Label,
        _nan_label: Label,
        _succeed_label: Label,
    ) {
        unimplemented!();
    }
    // Checks for underflow/overflow/nan before IxxTrunc{U/S}F64.. return offset/len for trap_overflow and trap_badconv
    fn emit_f64_int_conv_check_trap(&mut self, reg: NEON, lower_bound: f64, upper_bound: f64) {
        unimplemented!();
    }
    fn emit_f64_int_conv_check_sat<
        F1: FnOnce(&mut Self),
        F2: FnOnce(&mut Self),
        F3: FnOnce(&mut Self),
        F4: FnOnce(&mut Self),
    >(
        &mut self,
        _reg: NEON,
        _lower_bound: f64,
        _upper_bound: f64,
        _underflow_cb: F1,
        _overflow_cb: F2,
        _nan_cb: Option<F3>,
        _convert_cb: F4,
    ) {
        unimplemented!();
    }

    fn convert_i64_f64_u_s(&mut self, _loc: Location, _ret: Location) {
        unimplemented!();
    }
    fn convert_i64_f64_u_u(&mut self, _loc: Location, _ret: Location) {
        unimplemented!();
    }
    fn convert_i64_f64_s_s(&mut self, _loc: Location, _ret: Location) {
        unimplemented!();
    }
    fn convert_i64_f64_s_u(&mut self, _loc: Location, _ret: Location) {
        unimplemented!();
    }
    fn convert_i32_f64_s_s(&mut self, _loc: Location, _ret: Location) {
        unimplemented!();
    }
    fn convert_i32_f64_s_u(&mut self, _loc: Location, _ret: Location) {
        unimplemented!();
    }
    fn convert_i32_f64_u_s(&mut self, _loc: Location, _ret: Location) {
        unimplemented!();
    }
    fn convert_i32_f64_u_u(&mut self, _loc: Location, _ret: Location) {
        unimplemented!();
    }
    fn convert_i64_f32_u_s(&mut self, _loc: Location, _ret: Location) {
        unimplemented!();
    }
    fn convert_i64_f32_u_u(&mut self, _loc: Location, _ret: Location) {
        unimplemented!();
    }
    fn convert_i64_f32_s_s(&mut self, _loc: Location, _ret: Location) {
        unimplemented!();
    }
    fn convert_i64_f32_s_u(&mut self, _loc: Location, _ret: Location) {
        unimplemented!();
    }
    fn convert_i32_f32_s_s(&mut self, _loc: Location, _ret: Location) {
        unimplemented!();
    }
    fn convert_i32_f32_s_u(&mut self, _loc: Location, _ret: Location) {
        unimplemented!();
    }
    fn convert_i32_f32_u_s(&mut self, _loc: Location, _ret: Location) {
        unimplemented!();
    }
    fn convert_i32_f32_u_u(&mut self, _loc: Location, _ret: Location) {
        unimplemented!();
    }

    fn offset_is_ok(&self, size: Size, offset: i32) -> bool {
        if offset < 0 {
            return false;
        }
        let shift = match size {
            Size::S8 => 0,
            Size::S16 => 1,
            Size::S32 => 2,
            Size::S64 => 3,
        };
        if offset >= 0x1000 << shift {
            return false;
        }
        if (offset >> shift) << shift != offset {
            return false;
        }
        return true;
    }

    fn emit_push(&mut self, sz: Size, src: Location) {
        match (sz, src) {
            (Size::S64, Location::GPR(_)) | (Size::S64, Location::SIMD(_)) => {
                let offset = if self.pushed {
                    8
                } else {
                    self.assembler.emit_sub(
                        Size::S64,
                        Location::GPR(GPR::XzrSp),
                        Location::Imm8(16),
                        Location::GPR(GPR::XzrSp),
                    );
                    0
                };
                self.assembler
                    .emit_str(Size::S64, src, Location::Memory(GPR::XzrSp, offset));
                self.pushed = !self.pushed;
            }
            _ => panic!("singlepass can't emit PUSH {:?} {:?}", sz, src),
        }
    }
    fn emit_double_push(&mut self, sz: Size, src1: Location, src2: Location) {
        if !self.pushed {
            match (sz, src1, src2) {
                (Size::S64, Location::GPR(_), Location::GPR(_)) => {
                    self.assembler
                        .emit_stpbd(Size::S64, src1, src2, GPR::XzrSp, 16);
                }
                _ => {
                    self.emit_push(sz, src1);
                    self.emit_push(sz, src2);
                }
            }
        } else {
            self.emit_push(sz, src1);
            self.emit_push(sz, src2);
        }
    }
    fn emit_pop(&mut self, sz: Size, dst: Location) {
        match (sz, dst) {
            (Size::S64, Location::GPR(_)) | (Size::S64, Location::SIMD(_)) => {
                let offset = if self.pushed { 0 } else { 8 };
                self.assembler
                    .emit_ldr(Size::S64, dst, Location::Memory(GPR::XzrSp, offset));
                if self.pushed {
                    self.assembler.emit_add(
                        Size::S64,
                        Location::GPR(GPR::XzrSp),
                        Location::Imm8(16),
                        Location::GPR(GPR::XzrSp),
                    );
                }
                self.pushed = !self.pushed;
            }
            _ => panic!("singlepass can't emit PUSH {:?} {:?}", sz, dst),
        }
    }
    fn emit_double_pop(&mut self, sz: Size, dst1: Location, dst2: Location) {
        if !self.pushed {
            match (sz, dst1, dst2) {
                (Size::S64, Location::GPR(_), Location::GPR(_)) => {
                    self.assembler
                        .emit_ldpai(Size::S64, dst1, dst2, GPR::XzrSp, 16);
                }
                _ => {
                    self.emit_pop(sz, dst2);
                    self.emit_pop(sz, dst1);
                }
            }
        } else {
            self.emit_pop(sz, dst2);
            self.emit_pop(sz, dst1);
        }
    }
}

impl Machine for MachineARM64 {
    type GPR = GPR;
    type SIMD = NEON;
    fn assembler_get_offset(&self) -> Offset {
        self.assembler.get_offset()
    }
    fn index_from_gpr(&self, x: GPR) -> RegisterIndex {
        RegisterIndex(x as usize)
    }
    fn index_from_simd(&self, x: NEON) -> RegisterIndex {
        RegisterIndex(x as usize + 32)
    }

    fn get_vmctx_reg(&self) -> GPR {
        GPR::X28
    }

    fn get_used_gprs(&self) -> Vec<GPR> {
        self.used_gprs.iter().cloned().collect()
    }

    fn get_used_simd(&self) -> Vec<NEON> {
        self.used_simd.iter().cloned().collect()
    }

    fn pick_gpr(&self) -> Option<GPR> {
        use GPR::*;
        static REGS: &[GPR] = &[
            X0, X1, X2, X3, X4, X5, X6, X7, X8, X9, X10, X11, X12, X13, X14, X15,
        ];
        for r in REGS {
            if !self.used_gprs.contains(r) {
                return Some(*r);
            }
        }
        None
    }

    // Picks an unused general purpose register for internal temporary use.
    fn pick_temp_gpr(&self) -> Option<GPR> {
        use GPR::*;
        static REGS: &[GPR] = &[X0, X1, X2, X3, X4, X5, X6, X7];
        for r in REGS {
            if !self.used_gprs.contains(r) {
                return Some(*r);
            }
        }
        None
    }

    fn acquire_temp_gpr(&mut self) -> Option<GPR> {
        let gpr = self.pick_temp_gpr();
        if let Some(x) = gpr {
            self.used_gprs.insert(x);
        }
        gpr
    }

    fn release_gpr(&mut self, gpr: GPR) {
        assert!(self.used_gprs.remove(&gpr));
    }

    fn reserve_unused_temp_gpr(&mut self, gpr: GPR) -> GPR {
        assert!(!self.used_gprs.contains(&gpr));
        self.used_gprs.insert(gpr);
        gpr
    }

    fn reserve_gpr(&mut self, gpr: GPR) {
        self.used_gprs.insert(gpr);
    }

    fn push_used_gpr(&mut self) {
        let used_gprs = self.get_used_gprs();
        for r in used_gprs.iter() {
            self.emit_push(Size::S64, Location::GPR(*r));
        }
    }
    fn pop_used_gpr(&mut self) {
        let used_gprs = self.get_used_gprs();
        for r in used_gprs.iter().rev() {
            self.emit_pop(Size::S64, Location::GPR(*r));
        }
    }

    // Picks an unused NEON register.
    fn pick_simd(&self) -> Option<NEON> {
        use NEON::*;
        static REGS: &[NEON] = &[V8, V9, V10, V11, V12];
        for r in REGS {
            if !self.used_simd.contains(r) {
                return Some(*r);
            }
        }
        None
    }

    // Picks an unused NEON register for internal temporary use.
    fn pick_temp_simd(&self) -> Option<NEON> {
        use NEON::*;
        static REGS: &[NEON] = &[V0, V1, V2, V3, V4, V5, V6, V7];
        for r in REGS {
            if !self.used_simd.contains(r) {
                return Some(*r);
            }
        }
        None
    }

    // Acquires a temporary NEON register.
    fn acquire_temp_simd(&mut self) -> Option<NEON> {
        let simd = self.pick_temp_simd();
        if let Some(x) = simd {
            self.used_simd.insert(x);
        }
        simd
    }

    fn reserve_simd(&mut self, simd: NEON) {
        self.used_simd.insert(simd);
    }

    // Releases a temporary NEON register.
    fn release_simd(&mut self, simd: NEON) {
        assert_eq!(self.used_simd.remove(&simd), true);
    }

    fn push_used_simd(&mut self) {
        let used_neons = self.get_used_simd();
        self.adjust_stack((used_neons.len() * 8) as u32);

        for (i, r) in used_neons.iter().enumerate() {
            self.assembler.emit_str(
                Size::S64,
                Location::SIMD(*r),
                Location::Memory(GPR::XzrSp, (i * 8) as i32),
            );
        }
    }
    fn pop_used_simd(&mut self) {
        let used_neons = self.get_used_simd();
        for (i, r) in used_neons.iter().enumerate() {
            self.assembler.emit_ldr(
                Size::S64,
                Location::SIMD(*r),
                Location::Memory(GPR::XzrSp, (i * 8) as i32),
            );
        }
        let delta = if (used_neons.len() * 8) < 256 {
            Location::Imm8((used_neons.len() * 8) as u8)
        } else {
            let tmp = self.pick_temp_gpr().unwrap();
            self.assembler
                .emit_mov_imm(Location::GPR(tmp), (used_neons.len() * 8) as u64);
            Location::GPR(tmp)
        };
        self.assembler.emit_add(
            Size::S64,
            Location::GPR(GPR::XzrSp),
            delta,
            Location::GPR(GPR::XzrSp),
        );
    }

    /// Set the source location of the Wasm to the given offset.
    fn set_srcloc(&mut self, offset: u32) {
        self.src_loc = offset;
    }
    /// Marks each address in the code range emitted by `f` with the trap code `code`.
    fn mark_address_range_with_trap_code(&mut self, code: TrapCode, begin: usize, end: usize) {
        for i in begin..end {
            self.trap_table.offset_to_code.insert(i, code);
        }
        self.mark_instruction_address_end(begin);
    }

    /// Marks one address as trappable with trap code `code`.
    fn mark_address_with_trap_code(&mut self, code: TrapCode) {
        let offset = self.assembler.get_offset().0;
        self.trap_table.offset_to_code.insert(offset, code);
        self.mark_instruction_address_end(offset);
    }
    /// Marks the instruction as trappable with trap code `code`. return "begin" offset
    fn mark_instruction_with_trap_code(&mut self, code: TrapCode) -> usize {
        let offset = self.assembler.get_offset().0;
        self.trap_table.offset_to_code.insert(offset, code);
        offset
    }
    /// Pushes the instruction to the address map, calculating the offset from a
    /// provided beginning address.
    fn mark_instruction_address_end(&mut self, begin: usize) {
        self.instructions_address_map.push(InstructionAddressMap {
            srcloc: SourceLoc::new(self.src_loc),
            code_offset: begin,
            code_len: self.assembler.get_offset().0 - begin,
        });
    }

    /// Insert a StackOverflow (at offset 0)
    fn insert_stackoverflow(&mut self) {
        let offset = 0;
        self.trap_table
            .offset_to_code
            .insert(offset, TrapCode::StackOverflow);
        self.mark_instruction_address_end(offset);
    }

    /// Get all current TrapInformation
    fn collect_trap_information(&self) -> Vec<TrapInformation> {
        self.trap_table
            .offset_to_code
            .clone()
            .into_iter()
            .map(|(offset, code)| TrapInformation {
                code_offset: offset as u32,
                trap_code: code,
            })
            .collect()
    }

    fn instructions_address_map(&self) -> Vec<InstructionAddressMap> {
        self.instructions_address_map.clone()
    }

    // Memory location for a local on the stack
    fn local_on_stack(&mut self, stack_offset: i32) -> Location {
        Location::Memory(GPR::X27, -stack_offset)
    }

    // Adjust stack for locals
    fn adjust_stack(&mut self, delta_stack_offset: u32) {
        let delta = if delta_stack_offset < 256 {
            Location::Imm8(delta_stack_offset as u8)
        } else {
            let tmp = self.pick_temp_gpr().unwrap();
            self.assembler
                .emit_mov_imm(Location::GPR(tmp), delta_stack_offset as u64);
            Location::GPR(tmp)
        };
        self.assembler.emit_sub(
            Size::S64,
            Location::GPR(GPR::XzrSp),
            delta,
            Location::GPR(GPR::XzrSp),
        );
    }
    // restore stack
    fn restore_stack(&mut self, delta_stack_offset: u32) {
        let delta = if delta_stack_offset < 256 {
            Location::Imm8(delta_stack_offset as u8)
        } else {
            let tmp = self.pick_temp_gpr().unwrap();
            self.assembler
                .emit_mov_imm(Location::GPR(tmp), delta_stack_offset as u64);
            Location::GPR(tmp)
        };
        self.assembler.emit_add(
            Size::S64,
            Location::GPR(GPR::XzrSp),
            delta,
            Location::GPR(GPR::XzrSp),
        );
    }
    fn push_callee_saved(&mut self) {}
    fn pop_callee_saved(&mut self) {}
    fn pop_stack_locals(&mut self, delta_stack_offset: u32) {
        let delta = if delta_stack_offset < 256 {
            Location::Imm8(delta_stack_offset as u8)
        } else {
            let tmp = self.pick_temp_gpr().unwrap();
            self.assembler
                .emit_mov_imm(Location::GPR(tmp), delta_stack_offset as u64);
            Location::GPR(tmp)
        };
        self.assembler.emit_add(
            Size::S64,
            Location::GPR(GPR::XzrSp),
            delta,
            Location::GPR(GPR::XzrSp),
        );
    }
    // push a value on the stack for a native call
    fn push_location_for_native(&mut self, loc: Location) {
        match loc {
            Location::Imm64(_) => {
                self.reserve_unused_temp_gpr(GPR::X4);
                self.move_location(Size::S64, loc, Location::GPR(GPR::X4));
                self.emit_push(Size::S64, Location::GPR(GPR::X4));
                self.release_gpr(GPR::X4);
            }
            _ => self.emit_push(Size::S64, loc),
        }
    }

    // Zero a location that is 32bits
    fn zero_location(&mut self, size: Size, location: Location) {
        match location {
            Location::GPR(_) => self.assembler.emit_mov_imm(location, 0u64),
            _ => unreachable!(),
        }
    }

    // GPR Reg used for local pointer on the stack
    fn local_pointer(&self) -> GPR {
        GPR::X27
    }

    // Determine whether a local should be allocated on the stack.
    fn is_local_on_stack(&self, idx: usize) -> bool {
        idx > 7
    }

    // Determine a local's location.
    fn get_local_location(&self, idx: usize, callee_saved_regs_size: usize) -> Location {
        // Use callee-saved registers for the first locals.
        match idx {
            0 => Location::GPR(GPR::X18),
            1 => Location::GPR(GPR::X19),
            2 => Location::GPR(GPR::X20),
            3 => Location::GPR(GPR::X21),
            4 => Location::GPR(GPR::X22),
            5 => Location::GPR(GPR::X23),
            6 => Location::GPR(GPR::X24),
            7 => Location::GPR(GPR::X25),
            _ => Location::Memory(GPR::X27, -(((idx - 3) * 8 + callee_saved_regs_size) as i32)),
        }
    }
    // Move a local to the stack
    fn move_local(&mut self, stack_offset: i32, location: Location) {
        if stack_offset < 256 {
            self.assembler
                .emit_stur(Size::S64, location, GPR::X27, -stack_offset);
        } else {
            let tmp = self.pick_temp_gpr().unwrap();
            self.assembler
                .emit_mov_imm(Location::GPR(tmp), stack_offset as u64);
            self.assembler.emit_sub(
                Size::S64,
                Location::GPR(GPR::X27),
                Location::GPR(tmp),
                Location::GPR(tmp),
            );
            self.assembler
                .emit_str(Size::S64, location, Location::GPR(tmp));
        }
    }

    // List of register to save, depending on the CallingConvention
    fn list_to_save(&self, calling_convention: CallingConvention) -> Vec<Location> {
        vec![]
    }

    // Get param location
    fn get_param_location(&self, idx: usize, calling_convention: CallingConvention) -> Location {
        match calling_convention {
            _ => match idx {
                0 => Location::GPR(GPR::X0),
                1 => Location::GPR(GPR::X1),
                2 => Location::GPR(GPR::X2),
                3 => Location::GPR(GPR::X3),
                4 => Location::GPR(GPR::X4),
                5 => Location::GPR(GPR::X5),
                6 => Location::GPR(GPR::X6),
                7 => Location::GPR(GPR::X7),
                _ => Location::Memory(GPR::X27, (16 + (idx - 8) * 8) as i32),
            },
        }
    }
    // move a location to another
    fn move_location(&mut self, size: Size, source: Location, dest: Location) {
        match source {
            Location::GPR(_) | Location::SIMD(_) => match dest {
                Location::GPR(_) | Location::SIMD(_) => self.assembler.emit_mov(size, source, dest),
                Location::Memory(addr, offs) => {
                    if self.offset_is_ok(size, offs) {
                        self.assembler.emit_str(size, source, dest);
                    } else if offs > -256 && offs < 256 {
                        self.assembler.emit_stur(size, dest, addr, offs);
                    } else {
                        let tmp = self.pick_temp_gpr().unwrap();
                        if offs < 0 {
                            self.assembler
                                .emit_mov_imm(Location::GPR(tmp), (-offs) as u64);
                            self.assembler.emit_sub(
                                Size::S64,
                                Location::GPR(addr),
                                Location::GPR(tmp),
                                Location::GPR(tmp),
                            );
                        } else {
                            self.assembler.emit_mov_imm(Location::GPR(tmp), offs as u64);
                            self.assembler.emit_add(
                                Size::S64,
                                Location::GPR(addr),
                                Location::GPR(tmp),
                                Location::GPR(tmp),
                            );
                        }
                        self.assembler.emit_str(size, source, Location::GPR(tmp));
                    }
                }
                _ => panic!(
                    "singlepass can't emit move_location {:?} {:?} => {:?}",
                    size, source, dest
                ),
            },
            Location::Imm8(_) | Location::Imm32(_) | Location::Imm64(_) => match dest {
                Location::GPR(_) => self.assembler.emit_mov(size, source, dest),
                _ => panic!(
                    "singlepass can't emit move_location {:?} {:?} => {:?}",
                    size, source, dest
                ),
            },
            Location::Memory(addr, offs) => match dest {
                Location::GPR(_) => {
                    if self.offset_is_ok(size, offs) {
                        self.assembler.emit_ldr(size, dest, source);
                    } else if offs > -256 && offs < 256 {
                        self.assembler.emit_ldur(size, dest, addr, offs);
                    } else {
                        let tmp = self.pick_temp_gpr().unwrap();
                        if offs < 0 {
                            self.assembler
                                .emit_mov_imm(Location::GPR(tmp), (-offs) as u64);
                            self.assembler.emit_sub(
                                Size::S64,
                                Location::GPR(addr),
                                Location::GPR(tmp),
                                Location::GPR(tmp),
                            );
                        } else {
                            self.assembler.emit_mov_imm(Location::GPR(tmp), offs as u64);
                            self.assembler.emit_add(
                                Size::S64,
                                Location::GPR(addr),
                                Location::GPR(tmp),
                                Location::GPR(tmp),
                            );
                        }
                        self.assembler.emit_ldr(size, source, Location::GPR(tmp));
                    }
                }
                _ => panic!(
                    "singlepass can't emit move_location {:?} {:?} => {:?}",
                    size, source, dest
                ),
            },
            _ => panic!(
                "singlepass can't emit move_location {:?} {:?} => {:?}",
                size, source, dest
            ),
        }
    }
    // move a location to another
    fn move_location_extend(
        &mut self,
        size_val: Size,
        signed: bool,
        source: Location,
        size_op: Size,
        dest: Location,
    ) {
        unimplemented!();
    }
    fn load_address(&mut self, size: Size, reg: Location, mem: Location) {
        unimplemented!();
    }
    // Init the stack loc counter
    fn init_stack_loc(&mut self, init_stack_loc_cnt: u64, last_stack_loc: Location) {
        unimplemented!();
    }
    // Restore save_area
    fn restore_saved_area(&mut self, saved_area_offset: i32) {
        if saved_area_offset < 256 {
            self.assembler.emit_sub(
                Size::S64,
                Location::GPR(GPR::X27),
                Location::Imm8(saved_area_offset as u8),
                Location::GPR(GPR::XzrSp),
            );
        } else {
            let tmp = self.acquire_temp_gpr().unwrap();
            self.assembler
                .emit_mov_imm(Location::GPR(tmp), saved_area_offset as u64);
            self.assembler.emit_sub(
                Size::S64,
                Location::GPR(GPR::X27),
                Location::GPR(tmp),
                Location::GPR(GPR::XzrSp),
            );
            self.release_gpr(tmp);
        }
    }
    // Pop a location
    fn pop_location(&mut self, location: Location) {
        self.emit_pop(Size::S64, location);
    }
    // Create a new `MachineState` with default values.
    fn new_machine_state(&self) -> MachineState {
        new_machine_state()
    }

    // assembler finalize
    fn assembler_finalize(self) -> Vec<u8> {
        self.assembler.finalize().unwrap()
    }

    fn get_offset(&self) -> Offset {
        self.assembler.get_offset()
    }

    fn finalize_function(&mut self) {
        self.assembler.finalize_function();
    }

    fn emit_function_prolog(&mut self) {
        self.emit_double_push(Size::S64, Location::GPR(GPR::X27), Location::GPR(GPR::X30)); // save LR too
        self.move_location(
            Size::S64,
            Location::GPR(GPR::XzrSp),
            Location::GPR(GPR::X27),
        );
    }

    fn emit_function_epilog(&mut self) {
        self.move_location(
            Size::S64,
            Location::GPR(GPR::X27),
            Location::GPR(GPR::XzrSp),
        );
        self.emit_double_pop(Size::S64, Location::GPR(GPR::X27), Location::GPR(GPR::X30));
    }

    fn emit_function_return_value(&mut self, ty: WpType, canonicalize: bool, loc: Location) {
        if canonicalize {
            self.canonicalize_nan(
                match ty {
                    WpType::F32 => Size::S32,
                    WpType::F64 => Size::S64,
                    _ => unreachable!(),
                },
                loc,
                Location::GPR(GPR::X0),
            );
        } else {
            self.emit_relaxed_mov(Size::S64, loc, Location::GPR(GPR::X0));
        }
    }

    fn emit_function_return_float(&mut self) {
        self.move_location(Size::S64, Location::GPR(GPR::X0), Location::SIMD(NEON::V0));
    }

    fn arch_supports_canonicalize_nan(&self) -> bool {
        self.assembler.arch_supports_canonicalize_nan()
    }
    fn canonicalize_nan(&mut self, sz: Size, input: Location, output: Location) {
        unimplemented!();
    }

    fn emit_illegal_op(&mut self) {
        self.assembler.emit_udf();
    }
    fn get_label(&mut self) -> Label {
        self.assembler.new_dynamic_label()
    }
    fn emit_label(&mut self, label: Label) {
        self.assembler.emit_label(label);
    }
    fn get_grp_for_call(&self) -> GPR {
        GPR::X26
    }
    fn emit_call_register(&mut self, reg: GPR) {
        self.assembler.emit_call_register(reg);
    }
    fn emit_call_label(&mut self, label: Label) {
        self.assembler.emit_call_label(label);
    }
    fn get_gpr_for_ret(&self) -> GPR {
        GPR::X26
    }
    fn get_simd_for_ret(&self) -> NEON {
        NEON::V0
    }

    fn arch_requires_indirect_call_trampoline(&self) -> bool {
        self.assembler.arch_requires_indirect_call_trampoline()
    }

    fn arch_emit_indirect_call_with_trampoline(&mut self, location: Location) {
        self.assembler
            .arch_emit_indirect_call_with_trampoline(location);
    }

    fn emit_call_location(&mut self, location: Location) {
        unimplemented!();
    }

    fn location_address(&mut self, size: Size, source: Location, dest: Location) {
        unimplemented!();
    }
    // logic
    fn location_and(&mut self, size: Size, source: Location, dest: Location, _flags: bool) {
        unimplemented!();
    }
    fn location_xor(&mut self, size: Size, source: Location, dest: Location, _flags: bool) {
        unimplemented!();
    }
    fn location_or(&mut self, size: Size, source: Location, dest: Location, _flags: bool) {
        unimplemented!();
    }
    fn location_test(&mut self, size: Size, source: Location, dest: Location) {
        unimplemented!();
    }
    // math
    fn location_add(&mut self, size: Size, source: Location, dest: Location, _flags: bool) {
        unimplemented!();
    }
    fn location_sub(&mut self, size: Size, source: Location, dest: Location, _flags: bool) {
        unimplemented!();
    }
    fn location_cmp(&mut self, size: Size, source: Location, dest: Location) {
        unimplemented!();
    }
    // (un)conditionnal jmp
    // (un)conditionnal jmp
    fn jmp_unconditionnal(&mut self, label: Label) {
        self.assembler.emit_b_label(label);
    }
    fn jmp_on_equal(&mut self, label: Label) {
        self.assembler.emit_bcond_label(Condition::Eq, label);
    }
    fn jmp_on_different(&mut self, label: Label) {
        self.assembler.emit_bcond_label(Condition::Ne, label);
    }
    fn jmp_on_above(&mut self, label: Label) {
        self.assembler.emit_bcond_label(Condition::Hi, label);
    }
    fn jmp_on_aboveequal(&mut self, label: Label) {
        self.assembler.emit_bcond_label(Condition::Cs, label);
    }
    fn jmp_on_belowequal(&mut self, label: Label) {
        self.assembler.emit_bcond_label(Condition::Ls, label);
    }
    fn jmp_on_overflow(&mut self, label: Label) {
        self.assembler.emit_bcond_label(Condition::Cs, label);
    }

    // jmp table
    fn emit_jmp_to_jumptable(&mut self, label: Label, cond: Location) {
        unimplemented!();
    }

    fn align_for_loop(&mut self) {
        // noting to do on ARM64
    }

    fn emit_ret(&mut self) {
        self.assembler.emit_ret();
    }

    fn emit_push(&mut self, size: Size, loc: Location) {
        self.emit_push(size, loc);
    }
    fn emit_pop(&mut self, size: Size, loc: Location) {
        self.emit_pop(size, loc);
    }

    fn emit_memory_fence(&mut self) {
        // nothing on x86_64
    }

    fn location_neg(
        &mut self,
        size_val: Size, // size of src
        signed: bool,
        source: Location,
        size_op: Size,
        dest: Location,
    ) {
        unimplemented!();
    }

    fn emit_imul_imm32(&mut self, size: Size, imm32: u32, gpr: GPR) {
        unimplemented!();
    }

    // relaxed binop based...
    fn emit_relaxed_mov(&mut self, sz: Size, src: Location, dst: Location) {
        unimplemented!();
    }
    fn emit_relaxed_cmp(&mut self, sz: Size, src: Location, dst: Location) {
        unimplemented!();
    }
    fn emit_relaxed_zero_extension(
        &mut self,
        sz_src: Size,
        src: Location,
        sz_dst: Size,
        dst: Location,
    ) {
        unimplemented!();
    }
    fn emit_relaxed_sign_extension(
        &mut self,
        sz_src: Size,
        src: Location,
        sz_dst: Size,
        dst: Location,
    ) {
        unimplemented!();
    }

    fn emit_binop_add32(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        self.emit_binop_i32(Assembler::emit_add2, loc_a, loc_b, ret);
    }
    fn emit_binop_sub32(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn emit_binop_mul32(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn emit_binop_udiv32(
        &mut self,
        loc_a: Location,
        loc_b: Location,
        ret: Location,
        integer_division_by_zero: Label,
    ) -> usize {
        unimplemented!();
    }
    fn emit_binop_sdiv32(
        &mut self,
        loc_a: Location,
        loc_b: Location,
        ret: Location,
        integer_division_by_zero: Label,
    ) -> usize {
        unimplemented!();
    }
    fn emit_binop_urem32(
        &mut self,
        loc_a: Location,
        loc_b: Location,
        ret: Location,
        integer_division_by_zero: Label,
    ) -> usize {
        unimplemented!();
    }
    fn emit_binop_srem32(
        &mut self,
        loc_a: Location,
        loc_b: Location,
        ret: Location,
        integer_division_by_zero: Label,
    ) -> usize {
        unimplemented!();
    }
    fn emit_binop_and32(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn emit_binop_or32(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn emit_binop_xor32(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i32_cmp_ge_s(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i32_cmp_gt_s(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i32_cmp_le_s(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i32_cmp_lt_s(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i32_cmp_ge_u(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i32_cmp_gt_u(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i32_cmp_le_u(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i32_cmp_lt_u(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i32_cmp_ne(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i32_cmp_eq(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i32_clz(&mut self, loc: Location, ret: Location) {
        unimplemented!();
    }
    fn i32_ctz(&mut self, loc: Location, ret: Location) {
        unimplemented!();
    }
    fn i32_popcnt(&mut self, loc: Location, ret: Location) {
        unimplemented!();
    }
    fn i32_shl(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i32_shr(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i32_sar(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i32_rol(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i32_ror(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i32_load(
        &mut self,
        addr: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        self.memory_op(
            addr,
            memarg,
            false,
            4,
            need_check,
            imported_memories,
            offset,
            heap_access_oob,
            |this, addr| {
                this.assembler.emit_ldur(Size::S32, ret, addr, 0);
            },
        );
    }
    fn i32_load_8u(
        &mut self,
        addr: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        self.memory_op(
            addr,
            memarg,
            false,
            1,
            need_check,
            imported_memories,
            offset,
            heap_access_oob,
            |this, addr| {
                this.assembler.emit_ldrb(Size::S32, ret, addr, 0);
            },
        );
    }
    fn i32_load_8s(
        &mut self,
        addr: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        self.memory_op(
            addr,
            memarg,
            false,
            1,
            need_check,
            imported_memories,
            offset,
            heap_access_oob,
            |this, addr| {
                this.assembler.emit_ldrsb(Size::S32, ret, addr, 0);
            },
        );
    }
    fn i32_load_16u(
        &mut self,
        addr: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        self.memory_op(
            addr,
            memarg,
            false,
            2,
            need_check,
            imported_memories,
            offset,
            heap_access_oob,
            |this, addr| {
                this.assembler.emit_ldrh(Size::S32, ret, addr, 0);
            },
        );
    }
    fn i32_load_16s(
        &mut self,
        addr: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        self.memory_op(
            addr,
            memarg,
            false,
            2,
            need_check,
            imported_memories,
            offset,
            heap_access_oob,
            |this, addr| {
                this.assembler.emit_ldrsh(Size::S32, ret, addr, 0);
            },
        );
    }
    fn i32_atomic_load(
        &mut self,
        addr: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i32_atomic_load_8u(
        &mut self,
        addr: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i32_atomic_load_16u(
        &mut self,
        addr: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i32_save(
        &mut self,
        target_value: Location,
        memarg: &MemoryImmediate,
        target_addr: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i32_save_8(
        &mut self,
        target_value: Location,
        memarg: &MemoryImmediate,
        target_addr: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i32_save_16(
        &mut self,
        target_value: Location,
        memarg: &MemoryImmediate,
        target_addr: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i32_atomic_save(
        &mut self,
        value: Location,
        memarg: &MemoryImmediate,
        target_addr: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i32_atomic_save_8(
        &mut self,
        value: Location,
        memarg: &MemoryImmediate,
        target_addr: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i32_atomic_save_16(
        &mut self,
        value: Location,
        memarg: &MemoryImmediate,
        target_addr: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i32 atomic Add with i32
    fn i32_atomic_add(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i32 atomic Add with u8
    fn i32_atomic_add_8u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i32 atomic Add with u16
    fn i32_atomic_add_16u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i32 atomic Sub with i32
    fn i32_atomic_sub(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i32 atomic Sub with u8
    fn i32_atomic_sub_8u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i32 atomic Sub with u16
    fn i32_atomic_sub_16u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i32 atomic And with i32
    fn i32_atomic_and(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i32 atomic And with u8
    fn i32_atomic_and_8u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i32 atomic And with u16
    fn i32_atomic_and_16u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i32 atomic Or with i32
    fn i32_atomic_or(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i32 atomic Or with u8
    fn i32_atomic_or_8u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i32 atomic Or with u16
    fn i32_atomic_or_16u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i32 atomic Xor with i32
    fn i32_atomic_xor(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i32 atomic Xor with u8
    fn i32_atomic_xor_8u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i32 atomic Xor with u16
    fn i32_atomic_xor_16u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i32 atomic Exchange with i32
    fn i32_atomic_xchg(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i32 atomic Exchange with u8
    fn i32_atomic_xchg_8u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i32 atomic Exchange with u16
    fn i32_atomic_xchg_16u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i32 atomic Exchange with i32
    fn i32_atomic_cmpxchg(
        &mut self,
        new: Location,
        cmp: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i32 atomic Exchange with u8
    fn i32_atomic_cmpxchg_8u(
        &mut self,
        new: Location,
        cmp: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i32 atomic Exchange with u16
    fn i32_atomic_cmpxchg_16u(
        &mut self,
        new: Location,
        cmp: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }

    fn move_with_reloc(
        &mut self,
        reloc_target: RelocationTarget,
        relocations: &mut Vec<Relocation>,
    ) {
        let reloc_at = self.assembler.get_offset().0;
        relocations.push(Relocation {
            kind: RelocationKind::Arm64Movw0,
            reloc_target,
            offset: reloc_at as u32,
            addend: 0,
        });
        self.assembler.emit_movk(Location::GPR(GPR::X26), 0, 0);
        let reloc_at = self.assembler.get_offset().0;
        relocations.push(Relocation {
            kind: RelocationKind::Arm64Movw1,
            reloc_target,
            offset: reloc_at as u32,
            addend: 0,
        });
        self.assembler.emit_movk(Location::GPR(GPR::X26), 0, 16);
        let reloc_at = self.assembler.get_offset().0;
        relocations.push(Relocation {
            kind: RelocationKind::Arm64Movw2,
            reloc_target,
            offset: reloc_at as u32,
            addend: 0,
        });
        self.assembler.emit_movk(Location::GPR(GPR::X26), 0, 32);
        let reloc_at = self.assembler.get_offset().0;
        relocations.push(Relocation {
            kind: RelocationKind::Arm64Movw3,
            reloc_target,
            offset: reloc_at as u32,
            addend: 0,
        });
        self.assembler.emit_movk(Location::GPR(GPR::X26), 0, 48);
    }

    fn emit_binop_add64(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn emit_binop_sub64(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn emit_binop_mul64(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn emit_binop_udiv64(
        &mut self,
        loc_a: Location,
        loc_b: Location,
        ret: Location,
        integer_division_by_zero: Label,
    ) -> usize {
        unimplemented!();
    }
    fn emit_binop_sdiv64(
        &mut self,
        loc_a: Location,
        loc_b: Location,
        ret: Location,
        integer_division_by_zero: Label,
    ) -> usize {
        unimplemented!();
    }
    fn emit_binop_urem64(
        &mut self,
        loc_a: Location,
        loc_b: Location,
        ret: Location,
        integer_division_by_zero: Label,
    ) -> usize {
        unimplemented!();
    }
    fn emit_binop_srem64(
        &mut self,
        loc_a: Location,
        loc_b: Location,
        ret: Location,
        integer_division_by_zero: Label,
    ) -> usize {
        unimplemented!();
    }
    fn emit_binop_and64(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn emit_binop_or64(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn emit_binop_xor64(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i64_cmp_ge_s(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i64_cmp_gt_s(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i64_cmp_le_s(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i64_cmp_lt_s(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i64_cmp_ge_u(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i64_cmp_gt_u(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i64_cmp_le_u(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i64_cmp_lt_u(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i64_cmp_ne(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i64_cmp_eq(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i64_clz(&mut self, loc: Location, ret: Location) {
        unimplemented!();
    }
    fn i64_ctz(&mut self, loc: Location, ret: Location) {
        unimplemented!();
    }
    fn i64_popcnt(&mut self, loc: Location, ret: Location) {
        unimplemented!();
    }
    fn i64_shl(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i64_shr(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i64_sar(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i64_rol(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i64_ror(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn i64_load(
        &mut self,
        addr: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i64_load_8u(
        &mut self,
        addr: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i64_load_8s(
        &mut self,
        addr: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i64_load_16u(
        &mut self,
        addr: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i64_load_16s(
        &mut self,
        addr: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i64_load_32u(
        &mut self,
        addr: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i64_load_32s(
        &mut self,
        addr: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i64_atomic_load(
        &mut self,
        addr: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i64_atomic_load_8u(
        &mut self,
        addr: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i64_atomic_load_16u(
        &mut self,
        addr: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i64_atomic_load_32u(
        &mut self,
        addr: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i64_save(
        &mut self,
        target_value: Location,
        memarg: &MemoryImmediate,
        target_addr: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i64_save_8(
        &mut self,
        target_value: Location,
        memarg: &MemoryImmediate,
        target_addr: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i64_save_16(
        &mut self,
        target_value: Location,
        memarg: &MemoryImmediate,
        target_addr: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i64_save_32(
        &mut self,
        target_value: Location,
        memarg: &MemoryImmediate,
        target_addr: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i64_atomic_save(
        &mut self,
        value: Location,
        memarg: &MemoryImmediate,
        target_addr: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i64_atomic_save_8(
        &mut self,
        value: Location,
        memarg: &MemoryImmediate,
        target_addr: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i64_atomic_save_16(
        &mut self,
        value: Location,
        memarg: &MemoryImmediate,
        target_addr: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn i64_atomic_save_32(
        &mut self,
        value: Location,
        memarg: &MemoryImmediate,
        target_addr: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic Add with i64
    fn i64_atomic_add(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic Add with u8
    fn i64_atomic_add_8u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic Add with u16
    fn i64_atomic_add_16u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic Add with u32
    fn i64_atomic_add_32u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic Sub with i64
    fn i64_atomic_sub(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic Sub with u8
    fn i64_atomic_sub_8u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic Sub with u16
    fn i64_atomic_sub_16u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic Sub with u32
    fn i64_atomic_sub_32u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic And with i64
    fn i64_atomic_and(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic And with u8
    fn i64_atomic_and_8u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic And with u16
    fn i64_atomic_and_16u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic And with u32
    fn i64_atomic_and_32u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic Or with i64
    fn i64_atomic_or(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic Or with u8
    fn i64_atomic_or_8u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic Or with u16
    fn i64_atomic_or_16u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic Or with u32
    fn i64_atomic_or_32u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic xor with i64
    fn i64_atomic_xor(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic xor with u8
    fn i64_atomic_xor_8u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic xor with u16
    fn i64_atomic_xor_16u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic xor with u32
    fn i64_atomic_xor_32u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic Exchange with i64
    fn i64_atomic_xchg(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic Exchange with u8
    fn i64_atomic_xchg_8u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic Exchange with u16
    fn i64_atomic_xchg_16u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic Exchange with u32
    fn i64_atomic_xchg_32u(
        &mut self,
        loc: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic Exchange with i64
    fn i64_atomic_cmpxchg(
        &mut self,
        new: Location,
        cmp: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic Exchange with u8
    fn i64_atomic_cmpxchg_8u(
        &mut self,
        new: Location,
        cmp: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic Exchange with u16
    fn i64_atomic_cmpxchg_16u(
        &mut self,
        new: Location,
        cmp: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    // i64 atomic Exchange with u32
    fn i64_atomic_cmpxchg_32u(
        &mut self,
        new: Location,
        cmp: Location,
        target: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }

    fn f32_load(
        &mut self,
        addr: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn f32_save(
        &mut self,
        target_value: Location,
        memarg: &MemoryImmediate,
        target_addr: Location,
        canonicalize: bool,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn f64_load(
        &mut self,
        addr: Location,
        memarg: &MemoryImmediate,
        ret: Location,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }
    fn f64_save(
        &mut self,
        target_value: Location,
        memarg: &MemoryImmediate,
        target_addr: Location,
        canonicalize: bool,
        need_check: bool,
        imported_memories: bool,
        offset: i32,
        heap_access_oob: Label,
    ) {
        unimplemented!();
    }

    fn convert_f64_i64(&mut self, loc: Location, signed: bool, ret: Location) {
        unimplemented!();
    }
    fn convert_f64_i32(&mut self, loc: Location, signed: bool, ret: Location) {
        unimplemented!();
    }
    fn convert_f32_i64(&mut self, loc: Location, signed: bool, ret: Location) {
        unimplemented!();
    }
    fn convert_f32_i32(&mut self, loc: Location, signed: bool, ret: Location) {
        unimplemented!();
    }
    fn convert_i64_f64(&mut self, loc: Location, ret: Location, signed: bool, sat: bool) {
        match (signed, sat) {
            (false, true) => self.convert_i64_f64_u_s(loc, ret),
            (false, false) => self.convert_i64_f64_u_u(loc, ret),
            (true, true) => self.convert_i64_f64_s_s(loc, ret),
            (true, false) => self.convert_i64_f64_s_u(loc, ret),
        }
    }
    fn convert_i32_f64(&mut self, loc: Location, ret: Location, signed: bool, sat: bool) {
        match (signed, sat) {
            (false, true) => self.convert_i32_f64_u_s(loc, ret),
            (false, false) => self.convert_i32_f64_u_u(loc, ret),
            (true, true) => self.convert_i32_f64_s_s(loc, ret),
            (true, false) => self.convert_i32_f64_s_u(loc, ret),
        }
    }
    fn convert_i64_f32(&mut self, loc: Location, ret: Location, signed: bool, sat: bool) {
        match (signed, sat) {
            (false, true) => self.convert_i64_f32_u_s(loc, ret),
            (false, false) => self.convert_i64_f32_u_u(loc, ret),
            (true, true) => self.convert_i64_f32_s_s(loc, ret),
            (true, false) => self.convert_i64_f32_s_u(loc, ret),
        }
    }
    fn convert_i32_f32(&mut self, loc: Location, ret: Location, signed: bool, sat: bool) {
        match (signed, sat) {
            (false, true) => self.convert_i32_f32_u_s(loc, ret),
            (false, false) => self.convert_i32_f32_u_u(loc, ret),
            (true, true) => self.convert_i32_f32_s_s(loc, ret),
            (true, false) => self.convert_i32_f32_s_u(loc, ret),
        }
    }
    fn convert_f64_f32(&mut self, loc: Location, ret: Location) {
        unimplemented!();
    }
    fn convert_f32_f64(&mut self, loc: Location, ret: Location) {
        unimplemented!();
    }
    fn f64_neg(&mut self, loc: Location, ret: Location) {
        unimplemented!();
    }
    fn f64_abs(&mut self, loc: Location, ret: Location) {
        unimplemented!();
    }
    fn emit_i64_copysign(&mut self, tmp1: GPR, tmp2: GPR) {
        unimplemented!();
    }
    fn f64_sqrt(&mut self, loc: Location, ret: Location) {
        unimplemented!();
    }
    fn f64_trunc(&mut self, loc: Location, ret: Location) {
        unimplemented!();
    }
    fn f64_ceil(&mut self, loc: Location, ret: Location) {
        unimplemented!();
    }
    fn f64_floor(&mut self, loc: Location, ret: Location) {
        unimplemented!();
    }
    fn f64_nearest(&mut self, loc: Location, ret: Location) {
        unimplemented!();
    }
    fn f64_cmp_ge(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn f64_cmp_gt(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn f64_cmp_le(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn f64_cmp_lt(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn f64_cmp_ne(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn f64_cmp_eq(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn f64_min(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn f64_max(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn f64_add(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn f64_sub(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn f64_mul(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn f64_div(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn f32_neg(&mut self, loc: Location, ret: Location) {
        unimplemented!();
    }
    fn f32_abs(&mut self, loc: Location, ret: Location) {
        unimplemented!();
    }
    fn emit_i32_copysign(&mut self, tmp1: GPR, tmp2: GPR) {
        unimplemented!();
    }
    fn f32_sqrt(&mut self, loc: Location, ret: Location) {
        unimplemented!();
    }
    fn f32_trunc(&mut self, loc: Location, ret: Location) {
        unimplemented!();
    }
    fn f32_ceil(&mut self, loc: Location, ret: Location) {
        unimplemented!();
    }
    fn f32_floor(&mut self, loc: Location, ret: Location) {
        unimplemented!();
    }
    fn f32_nearest(&mut self, loc: Location, ret: Location) {
        unimplemented!();
    }
    fn f32_cmp_ge(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn f32_cmp_gt(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn f32_cmp_le(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn f32_cmp_lt(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn f32_cmp_ne(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn f32_cmp_eq(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn f32_min(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn f32_max(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn f32_add(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn f32_sub(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn f32_mul(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }
    fn f32_div(&mut self, loc_a: Location, loc_b: Location, ret: Location) {
        unimplemented!();
    }

    fn gen_std_trampoline(
        &self,
        sig: &FunctionType,
        calling_convention: CallingConvention,
    ) -> FunctionBody {
        gen_std_trampoline_arm64(sig, calling_convention)
    }
    // Generates dynamic import function call trampoline for a function type.
    fn gen_std_dynamic_import_trampoline(
        &self,
        vmoffsets: &VMOffsets,
        sig: &FunctionType,
        calling_convention: CallingConvention,
    ) -> FunctionBody {
        gen_std_dynamic_import_trampoline_arm64(vmoffsets, sig, calling_convention)
    }
    // Singlepass calls import functions through a trampoline.
    fn gen_import_call_trampoline(
        &self,
        vmoffsets: &VMOffsets,
        index: FunctionIndex,
        sig: &FunctionType,
        calling_convention: CallingConvention,
    ) -> CustomSection {
        gen_import_call_trampoline_arm64(vmoffsets, index, sig, calling_convention)
    }
}
