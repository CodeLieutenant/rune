//! A runtime-only context gives the same runtime as a full one.

prelude!();

use crate::{Context, Vm};

#[test]
fn runtime_only_context_has_the_same_functions_and_constants() -> Result<()> {
    let full = Context::with_default_modules()?.into_runtime();
    let lean = Context::with_default_modules_for_runtime()?.into_runtime();
    assert_eq!(full.hashes(), lean.hashes());
    Ok(())
}

#[test]
fn runtime_only_context_runs_a_unit_compiled_by_a_full_context() -> Result<()> {
    let context = Context::with_default_modules()?;
    let mut sources = crate::sources! {
        entry => {
            use std::collections::HashMap;

            pub fn main() {
                let map = HashMap::new();
                let words = [];
                for i in 0..10 {
                    map.insert(i, format!("v{}", i * 2));
                    words.push(i.to_string());
                }
                let joined = words.iter().map(|w| w + "!").collect::<Vec>();
                let o = #{ a: 1.5f64.clamp(0.0, 1.0), b: "x" + "x" + "x" };
                format!("{} {:?} {} {}", map.len(), joined.len(), o.a, o.b)
            }
        }
    };
    let unit = Arc::try_new(
        crate::prepare(&mut sources)
            .with_context(&context)
            .build()?,
    )?;

    let full = Arc::try_new(context.runtime()?)?;
    let lean = Arc::try_new(Context::with_default_modules_for_runtime()?.into_runtime())?;

    let a = Vm::new(full, Arc::clone(&unit)).call(["main"], ())?;
    let b = Vm::new(lean, unit).call(["main"], ())?;
    let (a, b) = (
        crate::from_value::<String>(a)?,
        crate::from_value::<String>(b)?,
    );
    assert_eq!(a, "10 10 1.0 xxx");
    assert_eq!(a, b);
    Ok(())
}

/// A worker that never compiles gets the unit as bytes. Format specifications
/// carry optional fields, which must decode to the values they encoded.
#[test]
#[cfg(feature = "byte-code")]
fn runtime_only_context_runs_an_encoded_unit() -> Result<()> {
    let context = Context::with_default_modules()?;
    let mut sources = crate::sources! {
        entry => {
            pub fn main() {
                let pi = 3.14159;
                format!("{:?}|{:>6}|{:<4}|{:.2}|{:08.3}|{:x}", [1, 2], "ab", 7, pi, pi, 255)
            }
        }
    };
    let unit = crate::prepare(&mut sources)
        .with_context(&context)
        .build()?;

    let bytes = musli::storage::to_vec(&unit).expect("unit should encode");
    let decoded: crate::Unit = musli::storage::from_slice(&bytes).expect("unit should decode");

    let lean = Arc::try_new(Context::with_default_modules_for_runtime()?.into_runtime())?;
    let output = Vm::new(lean, Arc::try_new(decoded)?).call(["main"], ())?;
    let output = crate::from_value::<String>(output)?;
    assert_eq!(output, "[1, 2]|    ab|7   |3.14|0003.142|ff");
    Ok(())
}
