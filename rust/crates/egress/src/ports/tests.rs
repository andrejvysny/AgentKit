use super::absolute;

#[test]
fn the_system_resolver_asks_for_the_absolute_name() {
    assert_eq!(absolute("pypi.org"), "pypi.org.");
    assert_eq!(absolute("localhost"), "localhost.");
    assert_eq!(absolute("pypi.org."), "pypi.org.", "never a double dot");
}
