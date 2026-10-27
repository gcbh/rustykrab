//! Guard the real startup composition: supplying tools directly to a worker
//! constructor cannot detect registration after the daemon snapshots its tools.
use std::process::Command;

#[test]
fn daemon_inventory_recovery_proves_the_operation_then_resumes_original_work() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let temp = tempfile::tempdir().unwrap();
    let output = Command::new("python3")
        .arg(root.join("scripts/verify_capability_recovery.py"))
        .args(["--binary", env!("CARGO_BIN_EXE_rustykrab-cli"), "--fixture"])
        .arg(temp.path().join("fixture"))
        .args(["--expect", "pass"])
        .output()
        .expect("start stdlib-only daemon fixture");
    assert!(
        output.status.success(),
        "actual daemon composition recovery failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
