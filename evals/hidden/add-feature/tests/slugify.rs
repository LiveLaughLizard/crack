// Copied in by the eval runner after the model is done, so the check can't be gamed.
use add_feature::slugify;

#[test]
fn slugify_basics() {
    assert_eq!(slugify("Hello, World!"), "hello-world");
    assert_eq!(slugify("  Rust  2024 edition "), "rust-2024-edition");
    assert_eq!(slugify("already-a-slug"), "already-a-slug");
    assert_eq!(slugify("--Trim__these--"), "trim-these");
    assert_eq!(slugify(""), "");
}
