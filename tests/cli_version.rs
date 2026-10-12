//! The npm shim's installer verifies a downloaded binary by running
//! `zoanthrope --version` and matching `zoanthrope <version>`. That
//! exact shape is a release contract — a change here breaks every
//! postinstall verification until the shim is updated too.

#[test]
fn version_flag_reports_cargo_package_version() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_zoanthrope"))
        .arg("--version")
        .output()
        .expect("zoanthrope binary runs");
    assert!(out.status.success(), "zoanthrope --version exits 0");
    let reported = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(
        reported.trim(),
        format!("zoanthrope {}", env!("CARGO_PKG_VERSION"))
    );
}
