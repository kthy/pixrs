#[cfg(test)]
use pretty_assertions::assert_eq;

#[test]
fn canary_in_the_coalmine() {
    assert_eq!(2 + 2, 4);
}

#[test]
#[should_panic(expected = "at the disco")]
fn panic_at_the_disco() {
    panic!("at the disco");
}
