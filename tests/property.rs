use hegel::TestCase;
use hegel::generators::integers;

#[hegel::test]
fn integer_addition_is_commutative(tc: TestCase) {
    let x = tc.draw(integers::<i64>());
    let y = tc.draw(integers::<i64>());
    assert_eq!(x.wrapping_add(y), y.wrapping_add(x));
}
