//! A function with a `JitEnter` instruction runs as native code for integer
//! arguments, and in the interpreter for anything else.

prelude!();

use crate::runtime::unit::UnitFn;
use crate::{Context, Hash, Unit, Vm};

fn build(source: &str) -> Result<(Context, Unit)> {
    let context = Context::with_default_modules()?;
    let mut sources = crate::tests::sources(source);
    let unit = crate::prepare(&mut sources)
        .with_context(&context)
        .build()?;
    Ok((context, unit))
}

fn offset(unit: &Unit, name: &str) -> usize {
    match unit.function(&Hash::type_hash([name])) {
        Some(UnitFn::Offset { offset, .. }) => *offset,
        _ => panic!("no function {name}"),
    }
}

fn run(context: &Context, unit: Unit, args: impl crate::runtime::GuardedArgs) -> Result<Value> {
    let mut vm = Vm::new(Arc::try_new(context.runtime()?)?, Arc::try_new(unit)?);
    Ok(vm.call(["main"], args)?)
}

extern "C" fn marked(a: i64, b: i64) -> i64 {
    a * 100 + b
}

extern "C" fn bails(_: i64, _: i64) -> i64 {
    crate::runtime::inst::JIT_BAIL
}

const SOURCE: &str = r#"
    fn add(a, b) { a + b }
    pub fn main(a, b) { add(a, b) }
"#;

#[test]
fn native_code_runs_for_integers() -> Result<()> {
    let (context, mut unit) = build(SOURCE)?;
    let at = offset(&unit, "add");
    // SAFETY: `marked` takes two i64 values and returns an i64.
    assert!(unsafe { unit.install_jit(at, 2, marked as *const () as usize as u64) });
    // SAFETY: as above.
    assert!(!unsafe { unit.install_jit(at, 2, marked as *const () as usize as u64) });

    let value: i64 = crate::from_value(run(&context, unit, (1i64, 2i64))?)?;
    assert_eq!(value, 102);
    Ok(())
}

#[test]
fn a_bail_runs_the_interpreter() -> Result<()> {
    let (context, mut unit) = build(SOURCE)?;
    let at = offset(&unit, "add");
    // SAFETY: `bails` takes two i64 values and returns an i64.
    assert!(unsafe { unit.install_jit(at, 2, bails as *const () as usize as u64) });

    let value: i64 = crate::from_value(run(&context, unit, (1i64, 2i64))?)?;
    assert_eq!(value, 3);
    Ok(())
}

#[test]
fn other_arguments_run_the_interpreter() -> Result<()> {
    let (context, mut unit) = build(SOURCE)?;
    let at = offset(&unit, "add");
    // SAFETY: `marked` takes two i64 values and returns an i64.
    assert!(unsafe { unit.install_jit(at, 2, marked as *const () as usize as u64) });

    let value: f64 = crate::from_value(run(&context, unit, (1.5f64, 2.0f64))?)?;
    assert_eq!(value, 3.5);
    Ok(())
}

#[test]
#[cfg(feature = "jit")]
fn the_translator_takes_pure_integer_functions() -> Result<()> {
    let (_, unit) = build(
        r#"
        fn gen(i) {
            let s = 0;
            for j in 0..8 { s = s + ((i + j) % 65536) * 40503 % 1000003; }
            s
        }
        fn twice(i) { gen(i) + gen(i + 1) }
        fn countdown(n) { let c = 0; while n > 0 { n = n - 1; c = c + 2; } c }
        fn fact(n) { if n < 2 { 1 } else { n * fact(n - 1) } }
        fn text(i) { `${i}` }
        pub fn main() { twice(1) + countdown(3) + fact(4) }
        "#,
    )?;

    let module = crate::runtime::jit::compile(&unit).expect("a module");
    assert_eq!(&module.bytes[..4], b"\0asm");

    let mut offsets: Vec<usize> = module.functions.iter().map(|f| f.offset).collect();
    offsets.sort_unstable();

    let mut expected = vec![
        offset(&unit, "gen"),
        offset(&unit, "twice"),
        offset(&unit, "countdown"),
    ];
    expected.sort_unstable();
    assert_eq!(offsets, expected);
    Ok(())
}
