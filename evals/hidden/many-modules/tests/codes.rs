#[test]
fn every_module_returns_its_code() {
    assert_eq!(many_modules::alpha::code(), "5B2C711C");
    assert_eq!(many_modules::bravo::code(), "EE6DD58C");
    assert_eq!(many_modules::charlie::code(), "B46BB77D");
    assert_eq!(many_modules::delta::code(), "5C0755C4");
    assert_eq!(many_modules::echo::code(), "DFC04EF0");
    assert_eq!(many_modules::foxtrot::code(), "BF5768BF");
    assert_eq!(many_modules::golf::code(), "33AFC9F6");
    assert_eq!(many_modules::hotel::code(), "6DEDB5D8");
    assert_eq!(many_modules::india::code(), "1CE09153");
    assert_eq!(many_modules::juliett::code(), "1EA596CC");
    assert_eq!(many_modules::kilo::code(), "345F0EF6");
    assert_eq!(many_modules::lima::code(), "5043787D");
}
