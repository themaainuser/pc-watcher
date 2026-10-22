//! Tests for the Win32 agent helpers (sys.rs).

#[test]
fn free_disk_bytes_on_temp_dir_is_positive() {
    assert!(pcw_foreground::sys::free_disk_bytes(&std::env::temp_dir()).unwrap() > 0);
}
