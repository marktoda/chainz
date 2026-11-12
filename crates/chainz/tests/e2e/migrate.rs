//! The automatic migration of a v1 config on the first run (the summary
//! and the files written are pinned in `golden.rs`).

use crate::common::*;

#[test]
fn a_v1_config_is_migrated_once_and_backed_up() {
    let sb = Sandbox::new();
    fs::create_dir_all(sb.dir()).unwrap();
    fs::write(sb.dir().join("config.json"), v1_fixture()).unwrap();
    sb.cmd()
        .arg("ls")
        .assert()
        .success()
        .stderr(predicate::str::contains("migrated 17 chains"));
    assert!(sb.config_path().exists());
    assert!(sb.dir().join("config.v1.json.bak").exists());
    assert!(!sb.dir().join("config.json").exists());
    // Second run: nothing to migrate, no notice.
    sb.cmd()
        .arg("ls")
        .assert()
        .success()
        .stderr(predicate::str::contains("migrated").not());
}

#[test]
fn a_legacy_home_config_is_migrated() {
    let sb = Sandbox::new();
    fs::write(sb.home().join(".chainz.json"), v1_fixture()).unwrap();
    sb.cmd()
        .arg("ls")
        .assert()
        .success()
        .stderr(predicate::str::contains("migrated 17 chains"));
    assert!(sb.config_path().exists());
    assert!(sb.home().join(".chainz.v1.json.bak").exists());
}

#[test]
fn migrated_config_never_prints_secrets() {
    let sb = Sandbox::new();
    fs::create_dir_all(sb.dir()).unwrap();
    fs::write(sb.dir().join("config.json"), v1_fixture()).unwrap();
    let out = sb.cmd().arg("ls").assert().success().get_output().clone();
    let secrets = fs::read_to_string(sb.secrets_path()).unwrap();
    let all = format!("{}{}", stdout(&out), String::from_utf8_lossy(&out.stderr));
    for line in secrets.lines() {
        if let Some((_, value)) = line.split_once(" = ") {
            let value = value.trim_matches('"');
            if value.len() >= 12 {
                assert!(!all.contains(value), "printed a secret value");
            }
        }
    }
}
