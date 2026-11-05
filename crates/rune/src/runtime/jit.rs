//! Translation of pure integer functions into a WebAssembly module.
//!
//! A WebAssembly guest cannot run code it generates. The guest gives the
//! module to its host, the host compiles it with its WebAssembly engine, and
//! the guest installs each function with [`Unit::install_jit`]. The engine
//! then runs the function as machine code instead of the interpreter.
//!
//! A translated function reads and writes only integers, booleans and integer
//! ranges, and calls only other translated functions. It has no side effects,
//! so when it meets a case it does not handle, such as an overflow, it returns
//! [`BAIL`] and the virtual machine runs the call again in the interpreter.

use rust_alloc::collections::BTreeMap;
use rust_alloc::format;
use rust_alloc::vec;
use rust_alloc::vec::Vec;

use wasm_encoder::{
    BlockType, CodeSection, ExportKind, ExportSection, Function, FunctionSection, Module,
    TypeSection, ValType,
};

use crate::runtime::inst::{InstArithmeticOp, InstOp, InstRange, InstValue, Kind};
use crate::runtime::unit::{UnitFn, UnitStorage};
use crate::runtime::{Address, Call, Output, Protocol, Unit};

pub use crate::runtime::inst::JIT_BAIL as BAIL;

/// The most arguments a translated function takes.
pub const MAX_ARGS: usize = 4;

/// A WebAssembly module with the translated functions of a unit.
///
/// The module has no imports. It exports function `k` of
/// [`JitModule::functions`] as `f{k}`, with `args` parameters of type `i64` and
/// one result of type `i64`.
#[derive(Debug)]
#[non_exhaustive]
pub struct JitModule {
    /// The encoded module.
    pub bytes: Vec<u8>,
    /// The translated functions, in the order of their exports.
    pub functions: Vec<JitFunction>,
}

/// A function of a unit which a [`JitModule`] exports.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct JitFunction {
    /// The instruction offset of the function in the unit.
    pub offset: usize,
    /// The number of arguments of the function.
    pub args: usize,
}

/// Translates every function of `unit` which this module supports.
///
/// Returns `None` when no function qualifies.
pub fn compile<S>(unit: &Unit<S>) -> Option<JitModule>
where
    S: UnitStorage,
{
    let mut starts = Vec::new();
    let mut candidates = BTreeMap::new();

    for (_, function) in unit.iter_functions() {
        if let UnitFn::Offset {
            offset,
            call,
            args,
            captures,
        } = *function
        {
            starts.push(offset);

            if matches!(call, Call::Immediate) && captures.is_none() && args <= MAX_ARGS {
                candidates.insert(offset, args);
            }
        }
    }

    starts.sort_unstable();
    let end = unit.instructions().end();

    let mut plans = BTreeMap::new();

    for (&offset, &args) in &candidates {
        let stop = starts
            .iter()
            .copied()
            .find(|&start| start > offset)
            .unwrap_or(end);

        if let Some(plan) = analyze(unit, offset, stop, args) {
            plans.insert(offset, plan);
        }
    }

    // A function qualifies when every function it calls qualifies first, which
    // leaves out recursion and calls to functions that stay interpreted.
    let mut order: Vec<usize> = Vec::new();

    loop {
        let before = order.len();

        for (&offset, plan) in &plans {
            if order.contains(&offset) {
                continue;
            }

            let ready = plan.callees.iter().all(|&(callee, args)| {
                order.contains(&callee) && plans.get(&callee).is_some_and(|p| p.args == args)
            });

            if ready {
                order.push(offset);
            }
        }

        if order.len() == before {
            break;
        }
    }

    if order.is_empty() {
        return None;
    }

    let index: BTreeMap<usize, u32> = order
        .iter()
        .enumerate()
        .map(|(n, &offset)| (offset, n as u32))
        .collect();

    let mut types = TypeSection::new();

    for args in 0..=MAX_ARGS {
        types
            .ty()
            .function(vec![ValType::I64; args], [ValType::I64]);
    }

    let mut functions = FunctionSection::new();
    let mut exports = ExportSection::new();
    let mut codes = CodeSection::new();
    let mut out = Vec::new();

    for (n, offset) in order.iter().enumerate() {
        let plan = &plans[offset];
        functions.function(plan.args as u32);
        exports.export(&format!("f{n}"), ExportKind::Func, n as u32);
        codes.function(&emit(plan, &index));
        out.push(JitFunction {
            offset: *offset,
            args: plan.args,
        });
    }

    let mut module = Module::new();
    module.section(&types);
    module.section(&functions);
    module.section(&exports);
    module.section(&codes);

    Some(JitModule {
        bytes: module.finish(),
        functions: out,
    })
}

/// What a frame slot holds at one point of a function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ty {
    /// Nothing was stored, or the value was moved or dropped.
    Unset,
    Int,
    Bool,
    /// An integer range. The start is in the slot, the end in its end local.
    Range,
    /// An iterator over an integer range. The next value is in the slot, the
    /// end in its end local.
    Iter,
    /// A copy of the iterator in the given slot, which shares its position.
    IterRef(usize),
    /// The `next` function of an iterator.
    NextFn,
    /// The result of `next` on the iterator in the given slot, which only the
    /// `IterNext` instruction right after the call reads.
    Next(usize),
    /// The paths which reach this point disagree.
    Conflict,
}

struct Plan {
    args: usize,
    size: usize,
    insts: Vec<Kind>,
    /// The index of the first instruction of each block, in order.
    leaders: Vec<usize>,
    /// The block which each jump target starts, by instruction index.
    targets: BTreeMap<usize, usize>,
    /// The type of the source of each copy or move.
    copies: BTreeMap<usize, Ty>,
    /// The iterator slot which each `IterNext` steps.
    owners: BTreeMap<usize, usize>,
    /// The functions which this one calls, with the number of arguments.
    callees: Vec<(usize, usize)>,
    /// The offset of the function which each call by hash calls.
    calls: BTreeMap<usize, usize>,
}

fn analyze<S>(unit: &Unit<S>, start: usize, stop: usize, args: usize) -> Option<Plan>
where
    S: UnitStorage,
{
    let mut ips = Vec::new();
    let mut insts = Vec::new();
    let mut ip = start;

    while ip < stop {
        let (inst, len) = unit.instruction_at(ip).ok()??;
        ips.push(ip);
        insts.push(inst.kind);
        ip = ip.checked_add(len)?;
    }

    let Some(&Kind::Allocate { size }) = insts.first() else {
        return None;
    };

    if size == 0 || size < args || insts.len() < 2 {
        return None;
    }

    // Jump targets as instruction indexes.
    let mut jumps = BTreeMap::new();

    for (n, kind) in insts.iter().enumerate() {
        let jump = match *kind {
            Kind::Jump { jump }
            | Kind::JumpIf { jump, .. }
            | Kind::JumpIfNot { jump, .. }
            | Kind::IterNext { jump, .. } => jump,
            _ => continue,
        };

        let target = unit.translate(jump).ok()?;
        let target = ips.binary_search(&target).ok()?;

        if target == 0 {
            return None;
        }

        jumps.insert(n, target);
    }

    let mut leaders = vec![1];

    for (n, kind) in insts.iter().enumerate() {
        if is_terminator(kind) && n + 1 < insts.len() {
            leaders.push(n + 1);
        }
    }

    leaders.extend(jumps.values().copied());
    leaders.sort_unstable();
    leaders.dedup();

    let block_of = |n: usize| leaders.partition_point(|&leader| leader <= n) - 1;

    let mut targets = BTreeMap::new();

    for (&n, &target) in &jumps {
        targets.insert(n, block_of(target));
    }

    let mut plan = Plan {
        args,
        size,
        insts,
        leaders,
        targets,
        copies: BTreeMap::new(),
        owners: BTreeMap::new(),
        callees: Vec::new(),
        calls: BTreeMap::new(),
    };

    let blocks = plan.leaders.len();
    let mut entry: Vec<Option<Vec<Ty>>> = vec![None; blocks];
    let mut first = vec![Ty::Unset; size];
    first[..args].fill(Ty::Int);
    entry[0] = Some(first);

    let mut pending = vec![0usize];
    let mut visits = 0usize;

    while let Some(block) = pending.pop() {
        visits += 1;

        if visits > blocks.saturating_mul(4 * size + 4) {
            return None;
        }

        let mut state = entry[block].clone()?;
        let from = plan.leaders[block];
        let to = plan
            .leaders
            .get(block + 1)
            .copied()
            .unwrap_or(plan.insts.len());
        let mut falls = true;

        for n in from..to {
            step(unit, &mut plan, &mut state, n)?;

            let kind = plan.insts[n];

            if let Some(&target) = plan.targets.get(&n) {
                merge(&mut entry, &mut pending, target, &state);
            }

            if matches!(kind, Kind::Jump { .. } | Kind::Return { .. }) {
                falls = false;
            }
        }

        if falls && block + 1 < blocks {
            merge(&mut entry, &mut pending, block + 1, &state);
        }
    }

    plan.callees.sort_unstable();
    plan.callees.dedup();
    Some(plan)
}

fn is_terminator(kind: &Kind) -> bool {
    matches!(
        kind,
        Kind::Jump { .. }
            | Kind::JumpIf { .. }
            | Kind::JumpIfNot { .. }
            | Kind::IterNext { .. }
            | Kind::Return { .. }
    )
}

fn merge(entry: &mut [Option<Vec<Ty>>], pending: &mut Vec<usize>, block: usize, state: &[Ty]) {
    match &mut entry[block] {
        Some(existing) => {
            let mut changed = false;

            for (a, b) in existing.iter_mut().zip(state) {
                if *a != *b && *a != Ty::Conflict {
                    *a = Ty::Conflict;
                    changed = true;
                }
            }

            if changed {
                pending.push(block);
            }
        }
        slot => {
            *slot = Some(state.to_vec());
            pending.push(block);
        }
    }
}

fn read(state: &[Ty], addr: Address) -> Option<Ty> {
    state.get(addr.offset()).copied()
}

fn write(state: &mut [Ty], out: Output, ty: Ty) -> Option<()> {
    if let Some(addr) = out.as_addr() {
        *state.get_mut(addr.offset())? = ty;
    }

    Some(())
}

fn expect(state: &[Ty], addr: Address, ty: Ty) -> Option<()> {
    (read(state, addr)? == ty).then_some(())
}

/// Records `value` for instruction `n`, which must agree with what an earlier
/// visit recorded.
fn note<T>(map: &mut BTreeMap<usize, T>, n: usize, value: T) -> Option<()>
where
    T: PartialEq,
{
    match map.get(&n) {
        Some(existing) if *existing != value => None,
        _ => {
            map.insert(n, value);
            Some(())
        }
    }
}

fn is_comparison(op: InstOp) -> bool {
    matches!(
        op,
        InstOp::Lt | InstOp::Le | InstOp::Gt | InstOp::Ge | InstOp::Eq | InstOp::Neq
    )
}

/// The iterator slot behind `addr`, for a call to `next` at instruction `n`
/// which the `IterNext` at `n + 1` reads.
fn next_call(plan: &Plan, state: &[Ty], n: usize, addr: Address, out: Output) -> Option<usize> {
    let owner = match read(state, addr)? {
        Ty::Iter => addr.offset(),
        Ty::IterRef(owner) => owner,
        _ => return None,
    };

    if *state.get(owner)? != Ty::Iter || plan.leaders.contains(&(n + 1)) {
        return None;
    }

    let Some(&Kind::IterNext { addr: next, .. }) = plan.insts.get(n + 1) else {
        return None;
    };

    (out.as_addr() == Some(next)).then_some(owner)
}

fn step<S>(unit: &Unit<S>, plan: &mut Plan, state: &mut [Ty], n: usize) -> Option<()>
where
    S: UnitStorage,
{
    match plan.insts[n] {
        Kind::Store { value, out } => {
            let ty = match value {
                InstValue::Integer(..) => Ty::Int,
                InstValue::Bool(..) => Ty::Bool,
                _ => return None,
            };

            write(state, out, ty)?;
        }
        Kind::Copy { addr, out } | Kind::Move { addr, out } => {
            let ty = read(state, addr)?;

            let copied = match ty {
                Ty::Int | Ty::Bool | Ty::Range | Ty::NextFn => ty,
                Ty::Iter => Ty::IterRef(addr.offset()),
                Ty::IterRef(owner) => Ty::IterRef(owner),
                _ => return None,
            };

            note(&mut plan.copies, n, ty)?;
            write(state, out, copied)?;
        }
        Kind::Arithmetic { a, b, out, .. } => {
            expect(state, a, Ty::Int)?;
            expect(state, b, Ty::Int)?;
            write(state, out, Ty::Int)?;
        }
        Kind::ArithmeticConst { a, b, out, .. } => {
            expect(state, a, Ty::Int)?;
            state.get(b.offset())?;
            write(state, out, Ty::Int)?;
        }
        Kind::Op { op, a, b, out } if is_comparison(op) => {
            expect(state, a, Ty::Int)?;
            expect(state, b, Ty::Int)?;
            write(state, out, Ty::Bool)?;
        }
        Kind::OpConst { op, a, b, out, .. } if is_comparison(op) => {
            expect(state, a, Ty::Int)?;
            state.get(b.offset())?;
            write(state, out, Ty::Bool)?;
        }
        Kind::Not { addr, out } => {
            expect(state, addr, Ty::Bool)?;
            write(state, out, Ty::Bool)?;
        }
        Kind::Neg { addr, out } => {
            expect(state, addr, Ty::Int)?;
            write(state, out, Ty::Int)?;
        }
        Kind::Jump { .. } => {}
        Kind::JumpIf { cond, .. } | Kind::JumpIfNot { cond, .. } => {
            expect(state, cond, Ty::Bool)?;
        }
        Kind::Range {
            range: InstRange::Range { start, end },
            out,
        } => {
            expect(state, start, Ty::Int)?;
            expect(state, end, Ty::Int)?;
            write(state, out, Ty::Range)?;
            out.as_addr()?;
        }
        Kind::CallAssociated {
            hash,
            addr,
            args: 1,
            out,
        } if hash == Protocol::INTO_ITER.hash => {
            expect(state, addr, Ty::Range)?;
            out.as_addr()?;
            write(state, out, Ty::Iter)?;
        }
        Kind::CallAssociated {
            hash,
            addr,
            args: 1,
            out,
        } if hash == Protocol::NEXT.hash => {
            let owner = next_call(plan, state, n, addr, out)?;
            write(state, out, Ty::Next(owner))?;
        }
        Kind::LoadInstanceFn { addr, hash, out } if hash == Protocol::NEXT.hash => {
            expect(state, addr, Ty::Iter)?;
            write(state, out, Ty::NextFn)?;
        }
        Kind::CallFn {
            function,
            addr,
            args: 1,
            out,
        } => {
            expect(state, function, Ty::NextFn)?;
            let owner = next_call(plan, state, n, addr, out)?;
            write(state, out, Ty::Next(owner))?;
        }
        Kind::IterNext { addr, out, .. } => {
            let Ty::Next(owner) = read(state, addr)? else {
                return None;
            };

            note(&mut plan.owners, n, owner)?;
            *state.get_mut(addr.offset())? = Ty::Unset;
            write(state, out, Ty::Int)?;
        }
        Kind::Drop { set } => {
            for addr in unit.lookup_drop_set(set)? {
                *state.get_mut(addr.offset())? = Ty::Unset;
            }
        }
        Kind::Return { addr } => {
            expect(state, addr, Ty::Int)?;
        }
        Kind::Call {
            hash,
            addr,
            args,
            out,
        } if args <= MAX_ARGS => {
            let Some(&UnitFn::Offset {
                offset,
                call: Call::Immediate,
                args: expected,
                captures: None,
            }) = unit.function(&hash)
            else {
                return None;
            };

            if expected != args {
                return None;
            }

            note(&mut plan.calls, n, offset)?;
            call(plan, state, offset, addr, args, out)?;
        }
        Kind::CallOffset {
            offset,
            call: Call::Immediate,
            addr,
            args,
            out,
        } if args <= MAX_ARGS => {
            call(plan, state, offset, addr, args, out)?;
        }
        _ => return None,
    }

    Some(())
}

/// Records a call to the function at `offset` with the integer arguments at
/// `addr`, which the call moves out of the frame.
fn call(
    plan: &mut Plan,
    state: &mut [Ty],
    offset: usize,
    addr: Address,
    args: usize,
    out: Output,
) -> Option<()> {
    for k in 0..args {
        let slot = addr.offset().checked_add(k)?;

        if *state.get(slot)? != Ty::Int {
            return None;
        }

        state[slot] = Ty::Unset;
    }

    plan.callees.push((offset, args));
    write(state, out, Ty::Int)
}

/// Local indexes of a translated function.
struct Locals {
    size: u32,
}

impl Locals {
    fn slot(&self, addr: Address) -> u32 {
        addr.offset() as u32
    }

    fn end(&self, slot: u32) -> u32 {
        self.size + slot
    }

    fn pc(&self) -> u32 {
        2 * self.size
    }

    fn a(&self) -> u32 {
        2 * self.size + 1
    }

    fn b(&self) -> u32 {
        2 * self.size + 2
    }

    fn r(&self) -> u32 {
        2 * self.size + 3
    }
}

/// Emits a function as a loop over its blocks. Each block is a case of a
/// `br_table` on the `pc` local, and a jump sets `pc` and branches to the loop.
fn emit(plan: &Plan, index: &BTreeMap<usize, u32>) -> Function {
    let size = plan.size;
    let l = Locals { size: size as u32 };

    let mut declared = vec![ValType::I64; 2 * size - plan.args];
    declared.push(ValType::I32);
    declared.extend([ValType::I64; 3]);

    let mut f = Function::new_with_locals_types(declared);
    let blocks = plan.leaders.len() as u32;

    f.instructions().loop_(BlockType::Empty);

    for _ in 0..blocks {
        f.instructions().block(BlockType::Empty);
    }

    f.instructions().local_get(l.pc()).br_table(0..blocks, 0);

    for block in 0..blocks as usize {
        f.instructions().end();

        // Branch depth to the loop from the code of this block.
        let depth = blocks - 1 - block as u32;
        let from = plan.leaders[block];
        let to = plan
            .leaders
            .get(block + 1)
            .copied()
            .unwrap_or(plan.insts.len());

        for n in from..to {
            emit_inst(&mut f, plan, index, &l, n, depth);
        }
    }

    f.instructions().end().i64_const(BAIL).end();
    f
}

fn bail(f: &mut Function) {
    f.instructions().i64_const(BAIL).return_();
}

fn bail_if(f: &mut Function) {
    f.instructions().if_(BlockType::Empty);
    bail(f);
    f.instructions().end();
}

fn jump_to(f: &mut Function, l: &Locals, block: usize, depth: u32) {
    f.instructions()
        .i32_const(block as i32)
        .local_set(l.pc())
        .br(depth);
}

fn store(f: &mut Function, l: &Locals, out: Output) {
    match out.as_addr() {
        Some(addr) => {
            f.instructions().local_set(l.slot(addr));
        }
        None => {
            f.instructions().drop();
        }
    }
}

fn emit_arithmetic(f: &mut Function, l: &Locals, op: InstArithmeticOp, out: Output) {
    let (a, b, r) = (l.a(), l.b(), l.r());

    match op {
        InstArithmeticOp::Add => {
            f.instructions()
                .local_get(a)
                .local_get(b)
                .i64_add()
                .local_set(r)
                .local_get(a)
                .local_get(r)
                .i64_xor()
                .local_get(b)
                .local_get(r)
                .i64_xor()
                .i64_and()
                .i64_const(0)
                .i64_lt_s();
            bail_if(f);
        }
        InstArithmeticOp::Sub => {
            f.instructions()
                .local_get(a)
                .local_get(b)
                .i64_sub()
                .local_set(r)
                .local_get(a)
                .local_get(b)
                .i64_xor()
                .local_get(a)
                .local_get(r)
                .i64_xor()
                .i64_and()
                .i64_const(0)
                .i64_lt_s();
            bail_if(f);
        }
        InstArithmeticOp::Mul => {
            // Two factors in the range of an i32 cannot overflow an i64. Other
            // factors run in the interpreter.
            f.instructions()
                .local_get(a)
                .i64_const(0x8000_0000)
                .i64_add()
                .i64_const(0xFFFF_FFFF)
                .i64_le_u()
                .local_get(b)
                .i64_const(0x8000_0000)
                .i64_add()
                .i64_const(0xFFFF_FFFF)
                .i64_le_u()
                .i32_and()
                .i32_eqz();
            bail_if(f);
            f.instructions()
                .local_get(a)
                .local_get(b)
                .i64_mul()
                .local_set(r);
        }
        InstArithmeticOp::Div | InstArithmeticOp::Rem => {
            f.instructions().local_get(b).i64_eqz();
            bail_if(f);
            f.instructions()
                .local_get(a)
                .i64_const(i64::MIN)
                .i64_eq()
                .local_get(b)
                .i64_const(-1)
                .i64_eq()
                .i32_and();
            bail_if(f);
            let mut sink = f.instructions();
            sink.local_get(a).local_get(b);

            if matches!(op, InstArithmeticOp::Div) {
                sink.i64_div_s();
            } else {
                sink.i64_rem_s();
            }

            sink.local_set(r);
        }
    }

    f.instructions().local_get(r);
    store(f, l, out);
}

fn emit_comparison(f: &mut Function, l: &Locals, op: InstOp, out: Output) {
    let mut sink = f.instructions();

    match op {
        InstOp::Lt => sink.i64_lt_s(),
        InstOp::Le => sink.i64_le_s(),
        InstOp::Gt => sink.i64_gt_s(),
        InstOp::Ge => sink.i64_ge_s(),
        InstOp::Eq => sink.i64_eq(),
        _ => sink.i64_ne(),
    };

    sink.i64_extend_i32_u();
    store(f, l, out);
}

fn emit_inst(
    f: &mut Function,
    plan: &Plan,
    index: &BTreeMap<usize, u32>,
    l: &Locals,
    n: usize,
    depth: u32,
) {
    match plan.insts[n] {
        Kind::Store { value, out } => {
            let value = match value {
                InstValue::Integer(value) => value,
                InstValue::Bool(value) => i64::from(value),
                _ => unreachable!(),
            };

            f.instructions().i64_const(value);
            store(f, l, out);
        }
        Kind::Copy { addr, out } | Kind::Move { addr, out } => match plan.copies[&n] {
            Ty::Int | Ty::Bool => {
                f.instructions().local_get(l.slot(addr));
                store(f, l, out);
            }
            Ty::Range => {
                if let Some(to) = out.as_addr() {
                    let (from, to) = (l.slot(addr), l.slot(to));
                    f.instructions()
                        .local_get(from)
                        .local_set(to)
                        .local_get(l.end(from))
                        .local_set(l.end(to));
                }
            }
            _ => {}
        },
        Kind::Arithmetic { op, a, b, out } => {
            f.instructions()
                .local_get(l.slot(a))
                .local_set(l.a())
                .local_get(l.slot(b))
                .local_set(l.b());
            emit_arithmetic(f, l, op, out);
        }
        Kind::ArithmeticConst {
            op, a, value, out, ..
        } => {
            f.instructions()
                .local_get(l.slot(a))
                .local_set(l.a())
                .i64_const(value)
                .local_set(l.b());
            emit_arithmetic(f, l, op, out);
        }
        Kind::Op { op, a, b, out } => {
            f.instructions().local_get(l.slot(a)).local_get(l.slot(b));
            emit_comparison(f, l, op, out);
        }
        Kind::OpConst {
            op, a, value, out, ..
        } => {
            f.instructions().local_get(l.slot(a)).i64_const(value);
            emit_comparison(f, l, op, out);
        }
        Kind::Not { addr, out } => {
            f.instructions()
                .local_get(l.slot(addr))
                .i64_eqz()
                .i64_extend_i32_u();
            store(f, l, out);
        }
        Kind::Neg { addr, out } => {
            f.instructions()
                .local_get(l.slot(addr))
                .i64_const(i64::MIN)
                .i64_eq();
            bail_if(f);
            f.instructions()
                .i64_const(0)
                .local_get(l.slot(addr))
                .i64_sub();
            store(f, l, out);
        }
        Kind::Jump { .. } => {
            jump_to(f, l, plan.targets[&n], depth);
        }
        Kind::JumpIf { cond, .. } => {
            f.instructions()
                .local_get(l.slot(cond))
                .i32_wrap_i64()
                .if_(BlockType::Empty);
            jump_to(f, l, plan.targets[&n], depth + 1);
            f.instructions().end();
        }
        Kind::JumpIfNot { cond, .. } => {
            f.instructions()
                .local_get(l.slot(cond))
                .i64_eqz()
                .if_(BlockType::Empty);
            jump_to(f, l, plan.targets[&n], depth + 1);
            f.instructions().end();
        }
        Kind::Range {
            range: InstRange::Range { start, end },
            out,
        } => {
            if let Some(to) = out.as_addr() {
                let to = l.slot(to);
                f.instructions()
                    .local_get(l.slot(start))
                    .local_set(to)
                    .local_get(l.slot(end))
                    .local_set(l.end(to));
            }
        }
        Kind::CallAssociated {
            hash, addr, out, ..
        } if hash == Protocol::INTO_ITER.hash => {
            if let Some(to) = out.as_addr() {
                let (from, to) = (l.slot(addr), l.slot(to));
                f.instructions()
                    .local_get(from)
                    .local_set(to)
                    .local_get(l.end(from))
                    .local_set(l.end(to));
            }
        }
        Kind::IterNext { out, .. } => {
            let owner = plan.owners[&n] as u32;
            f.instructions()
                .local_get(owner)
                .local_get(l.end(owner))
                .i64_ge_s()
                .if_(BlockType::Empty);
            jump_to(f, l, plan.targets[&n], depth + 1);
            f.instructions().end();

            if let Some(to) = out.as_addr() {
                f.instructions().local_get(owner).local_set(l.slot(to));
            }

            f.instructions()
                .local_get(owner)
                .i64_const(1)
                .i64_add()
                .local_set(owner);
        }
        Kind::Return { addr } => {
            f.instructions().local_get(l.slot(addr)).return_();
        }
        Kind::Call {
            addr, args, out, ..
        } => {
            emit_call(f, index, l, plan.calls[&n], addr, args, out);
        }
        Kind::CallOffset {
            offset,
            addr,
            args,
            out,
            ..
        } => {
            emit_call(f, index, l, offset, addr, args, out);
        }
        // Loading `next`, calling it and dropping emit nothing: `IterNext`
        // steps the iterator, and integers need no drop.
        _ => {}
    }
}

/// Calls a translated function and bails when it bails.
fn emit_call(
    f: &mut Function,
    index: &BTreeMap<usize, u32>,
    l: &Locals,
    offset: usize,
    addr: Address,
    args: usize,
    out: Output,
) {
    let mut sink = f.instructions();

    for k in 0..args {
        sink.local_get((addr.offset() + k) as u32);
    }

    sink.call(index[&offset])
        .local_tee(l.r())
        .i64_const(BAIL)
        .i64_eq();
    bail_if(f);
    f.instructions().local_get(l.r());
    store(f, l, out);
}
