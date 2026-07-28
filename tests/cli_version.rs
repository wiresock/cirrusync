use std::process::Command;

#[test]
fn version_flag_reports_the_package_version_without_configuration() {
    let output = Command::new(env!("CARGO_BIN_EXE_cirrusync"))
        .arg("--version")
        .output()
        .expect("cirrusync --version should execute");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("version output should be UTF-8"),
        format!("cirrusync {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(output.stderr.is_empty());
}
