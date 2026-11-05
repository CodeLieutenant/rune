//! A binary operation with a constant integer on the right-hand side runs as
//! one instruction. A first operand which is not a signed integer takes the
//! general path, with the same results as the two instructions it replaces.

prelude!();

use VmErrorKind::*;

#[test]
fn signed_operands() {
    let out: i64 = rune!(let a = 17; a + 3 - 1);
    assert_eq!(out, 19);

    let out: i64 = rune!(let a = 17; (a * 4) / 3 % 5);
    assert_eq!(out, 2);

    let out: Vec<bool> = rune!(let a = 7; [a < 8, a <= 7, a > 7, a >= 8, a == 7, a != 7]);
    assert_eq!(out, [true, true, false, false, true, false]);
}

#[test]
fn general_operands() {
    let out: u64 = rune!(let a = 17u64; a + 3);
    assert_eq!(out, 20);

    let out: Vec<bool> = rune!(let a = 7u64; [a == 7, a < 8, a > 8]);
    assert_eq!(out, [true, true, false]);

    let out: i64 = rune!(
        let n = 0;
        let j = 0;
        while j < 8 { n = n + j % 3; j = j + 1; }
        n
    );
    assert_eq!(out, 7);
}

#[test]
fn errors() {
    assert_vm_error!(
        "let a = i64::MAX; a + 1",
        Overflow => {}
    );

    assert_vm_error!(
        "let a = 10; a / 0",
        DivideByZero => {}
    );

    assert_vm_error!(
        "let a = 1.5; a + 1",
        UnsupportedBinaryOperation { .. } => {}
    );
}
