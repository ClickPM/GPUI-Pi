#[test]
fn runtime_fake_child_is_available_to_runtime_tests() {
    let binary = std::path::Path::new(env!("CARGO_BIN_EXE_runtime_fake_child"));
    assert!(binary.is_file(), "missing {}", binary.display());
}
