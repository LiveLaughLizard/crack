use super::*;

#[test]
fn names_and_expansion() {
    assert_eq!(
        exposed_name("git hub", "create.issue"),
        "mcp__git_hub__create_issue"
    );
    assert!(exposed_name("s", &"x".repeat(100)).len() <= 64);
    // SAFETY: only this test touches the variable.
    unsafe { std::env::set_var("CRACK_TEST_TOKEN", "abc") };
    assert_eq!(expand("Bearer ${CRACK_TEST_TOKEN}"), "Bearer abc");
    assert_eq!(expand("${CRACK_MISSING:-fallback}/x"), "fallback/x");
}
