//! `qb`'s exit codes for what it decides on its own, before any pipe is opened.
//! (Requests that reach QuickByte are covered by the core's pipe and processor
//! tests on pipe names of their own; these never touch the real command pipe.)

use std::process::Command;

fn qb(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_qb")).args(args).output().expect("qb runs")
}

#[test]
fn help_and_version_exit_zero() {
    let help = qb(&["help"]);
    assert_eq!(help.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&help.stdout).contains("Exit codes"));

    let version = qb(&["--version"]);
    assert_eq!(version.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("qb "));
}

#[test]
fn a_usage_mistake_exits_two_and_says_where_to_look() {
    for args in [&["frobnicate"][..], &["add"][..], &["pause"][..]] {
        let output = qb(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("qb help"), "{args:?}");
    }
}
