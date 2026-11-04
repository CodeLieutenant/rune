//! Machines that hold no values: `VmTs` for one owner, `VmPool` for many
//! threads.

prelude!();

use std::thread;

use futures_executor::block_on;

use crate::runtime::{Entry, VmPool, VmTs};
use crate::{Context, Vm};

fn compile() -> Result<(Arc<runtime::RuntimeContext>, Arc<crate::Unit>)> {
    let context = Context::with_default_modules()?;
    let mut sources = crate::sources! {
        entry => {
            pub fn add(a, b) { a + b }
            pub async fn twice(a) { a * 2 }
        }
    };
    let unit = crate::prepare(&mut sources)
        .with_context(&context)
        .build()?;
    Ok((Arc::try_new(context.runtime()?)?, Arc::try_new(unit)?))
}

fn entry(context: &Arc<runtime::RuntimeContext>, unit: &Arc<crate::Unit>, name: &str) -> Entry {
    Vm::new(Arc::clone(context), Arc::clone(unit))
        .lookup_entry([name])
        .unwrap()
}

#[test]
fn vm_ts_runs_calls_and_comes_back_cleared() -> Result<()> {
    let (context, unit) = compile()?;
    let add = entry(&context, &unit, "add");
    let mut vm = VmTs::new(Arc::clone(&context), Arc::clone(&unit));

    for i in 0..10i64 {
        let (result, back) = block_on(vm.execute_entry(&add, (i, 1i64))?.complete());
        assert_eq!(result?.as_signed()?, i + 1);
        vm = back;
    }

    Ok(())
}

#[test]
fn vm_pool_reuses_its_machines() -> Result<()> {
    let (context, unit) = compile()?;
    let twice = entry(&context, &unit, "twice");
    let pool = VmPool::new(Arc::clone(&context), Arc::clone(&unit), 3);
    assert_eq!(pool.capacity(), 4);

    let vm = pool.take();
    let (result, vm) = block_on(vm.execute_entry(&twice, (21i64,))?.complete());
    assert_eq!(result?.as_signed()?, 42);
    let first = vm.addr();
    pool.put(vm);

    let again = pool.take();
    assert_eq!(again.addr(), first, "the pool hands back its machine");
    Ok(())
}

#[test]
fn vm_pool_refuses_a_machine_of_another_unit() -> Result<()> {
    let (context, unit) = compile()?;
    let (other_context, other_unit) = compile()?;
    let pool = VmPool::new(Arc::clone(&context), Arc::clone(&unit), 1);

    // Both machines are alive at once, so their addresses differ.
    let own = VmTs::new(Arc::clone(&context), Arc::clone(&unit));
    let own_addr = own.addr();
    let foreign = VmTs::new(other_context, other_unit);

    // The pool has one slot. Had it taken the foreign machine, it would be
    // full and drop its own machine instead of keeping it.
    pool.put(foreign);
    pool.put(own);
    assert_eq!(pool.take().addr(), own_addr);
    Ok(())
}

#[test]
fn vm_pool_is_shared_by_threads() -> Result<()> {
    let (context, unit) = compile()?;
    let add = entry(&context, &unit, "add");
    let pool = Arc::try_new(VmPool::new(Arc::clone(&context), Arc::clone(&unit), 8))?;

    thread::scope(|scope| {
        for t in 0..8i64 {
            let pool = Arc::clone(&pool);
            scope.spawn(move || {
                for i in 0..1000i64 {
                    let vm = pool.take();
                    let execution = vm.execute_entry(&add, (t, i)).unwrap();
                    let (result, vm) = block_on(execution.complete());
                    assert_eq!(result.unwrap().as_signed().unwrap(), t + i);
                    pool.put(vm);
                }
            });
        }
    });

    Ok(())
}

#[test]
fn vm_is_small() {
    // The machine keeps no inline worklist slots, which halves it from the
    // 256 bytes it took with eight.
    assert!(
        core::mem::size_of::<Vm>() <= 128,
        "Vm is {} bytes",
        core::mem::size_of::<Vm>()
    );
}
