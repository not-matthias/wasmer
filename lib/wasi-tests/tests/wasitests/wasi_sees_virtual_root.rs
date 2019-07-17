#[test]
fn test_wasi_sees_virtual_root() {
    assert_wasi_output!(
        "../../wasitests/wasi_sees_virtual_root.wasm",
        "wasi_sees_virtual_root",
        vec![],
        vec![("act1".to_string(), ::std::path::PathBuf::from("wasitests/test_fs/hamlet/act1")),("act2".to_string(), ::std::path::PathBuf::from("wasitests/test_fs/hamlet/act2")),("act1-again".to_string(), ::std::path::PathBuf::from("wasitests/test_fs/hamlet/act1")),],
        vec![],
        "../../wasitests/wasi_sees_virtual_root.out"
    );
}
