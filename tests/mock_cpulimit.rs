use std::fs;
use std::process::{Command, Stdio};

use tempfile::tempdir;

#[test]
fn top_once_uses_mock_cpulimit_and_records_state() {
    let dir = tempdir().expect("tempdir");
    let cfg_dir = dir.path().join("cfg");
    let log_file = dir.path().join("cpulimit.log");
    let mock_bin = dir.path().join("cpulimit-mock.sh");

    fs::create_dir_all(&cfg_dir).expect("create cfg dir");
    fs::write(
        &mock_bin,
        "#!/bin/sh\nif [ \"$1\" = \"--help\" ]; then\n  echo \"Usage: cpulimit [OPTIONS...] TARGET\"\n  exit 1\nfi\necho \"$@\" >> \"$CPULIMIT_TOP_TEST_LOG\"\nsleep 2\n",
    )
    .expect("write mock bin");

    Command::new("chmod")
        .args(["+x", mock_bin.to_string_lossy().as_ref()])
        .status()
        .expect("chmod mock");

    let target_pid = std::process::id();
    let output = Command::new(env!("CARGO_BIN_EXE_cpulimit-top"))
        .args([
            "top",
            "--once",
            "--pid",
            &target_pid.to_string(),
            "--limit",
            "17",
        ])
        .env("CPULIMIT_TOP_CONFIG_DIR", &cfg_dir)
        .env("CPULIMIT_TOP_CPULIMIT_BIN", &mock_bin)
        .env("CPULIMIT_TOP_TEST_LOG", &log_file)
        .output()
        .expect("run binary");

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let state_file = cfg_dir.join("state.json");
    let state_text = fs::read_to_string(state_file).expect("read state");
    assert!(state_text.contains("\"mode\": \"adhoc\""));
    let state_json: serde_json::Value = serde_json::from_str(&state_text).expect("parse state");
    let cpulimit_pid = state_json["instances"][0]["cpulimit_pid"]
        .as_u64()
        .expect("cpulimit pid") as u32;
    let _ = Command::new("kill")
        .args(["-TERM", &cpulimit_pid.to_string()])
        .status();

    let log_text = fs::read_to_string(log_file).expect("read log");
    assert!(log_text.contains("-p"));
    assert!(log_text.contains("17"));
}

#[test]
fn top_default_creates_watch_rule() {
    let dir = tempdir().expect("tempdir");
    let cfg_dir = dir.path().join("cfg");
    let agents_dir = dir.path().join("agents");
    let mock_bin = dir.path().join("cpulimit-mock.sh");
    fs::create_dir_all(&cfg_dir).expect("create cfg dir");
    fs::create_dir_all(&agents_dir).expect("create agents dir");
    fs::write(
        &mock_bin,
        "#!/bin/sh\nif [ \"$1\" = \"--help\" ]; then\n  echo \"Usage: cpulimit [OPTIONS...] TARGET\"\n  exit 1\nfi\nexit 0\n",
    )
    .expect("write mock bin");
    Command::new("chmod")
        .args(["+x", mock_bin.to_string_lossy().as_ref()])
        .status()
        .expect("chmod mock");

    let mut sleeper = Command::new("sleep")
        .arg("5")
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn sleep");
    let target_pid = sleeper.id();
    let output = Command::new(env!("CARGO_BIN_EXE_cpulimit-top"))
        .args(["top", "--pid", &target_pid.to_string(), "--limit", "33"])
        .env("CPULIMIT_TOP_CONFIG_DIR", &cfg_dir)
        .env("CPULIMIT_TOP_CPULIMIT_BIN", &mock_bin)
        .env("CPULIMIT_TOP_DISABLE_LAUNCHD", "1")
        .env("CPULIMIT_TOP_LAUNCH_AGENTS_DIR", &agents_dir)
        .output()
        .expect("run binary");

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rules_text = fs::read_to_string(cfg_dir.join("rules.toml")).expect("read rules");
    assert!(rules_text.contains("limit = 33"));
    let _ = sleeper.kill();
    let _ = sleeper.wait();
}

#[test]
fn watches_shows_table_and_waiting_target() {
    let dir = tempdir().expect("tempdir");
    let cfg_dir = dir.path().join("cfg");
    fs::create_dir_all(&cfg_dir).expect("create cfg dir");

    fs::write(
        cfg_dir.join("rules.toml"),
        r#"
version = 1
[[rules]]
name = "definitely_not_running_proc_123"
limit = 21
domain = "user"
created_at = "2026-03-05T11:00:00+08:00"
updated_at = "2026-03-05T11:00:00+08:00"
"#,
    )
    .expect("write rules");

    let output = Command::new(env!("CARGO_BIN_EXE_cpulimit-top"))
        .arg("watches")
        .env("CPULIMIT_TOP_CONFIG_DIR", &cfg_dir)
        .env("CPULIMIT_TOP_DISABLE_LAUNCHD", "1")
        .output()
        .expect("run watches");

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("NAME"));
    assert!(stdout.contains("LAUNCHD"));
    assert!(stdout.contains("waiting"));
}

#[test]
fn status_shows_table_and_running_state() {
    let dir = tempdir().expect("tempdir");
    let cfg_dir = dir.path().join("cfg");
    fs::create_dir_all(&cfg_dir).expect("create cfg dir");

    let self_pid = std::process::id();
    fs::write(
        cfg_dir.join("state.json"),
        format!(
            r#"{{
  "version": 1,
  "instances": [
    {{
      "id": "ins_test",
      "mode": "adhoc",
      "cpulimit_pid": {self_pid},
      "target": {{ "kind": "pid", "value": {self_pid} }},
      "domain": "user",
      "started_at": "2026-03-05T11:00:00+08:00",
      "owner_label": null
    }}
  ]
}}"#
        ),
    )
    .expect("write state");

    let output = Command::new(env!("CARGO_BIN_EXE_cpulimit-top"))
        .arg("status")
        .env("CPULIMIT_TOP_CONFIG_DIR", &cfg_dir)
        .output()
        .expect("run status");

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("CPULIMIT"));
    assert!(stdout.contains("ins_test"));
    assert!(stdout.contains("running"));
}

#[test]
fn watches_should_not_leak_launchctl_output() {
    let dir = tempdir().expect("tempdir");
    let cfg_dir = dir.path().join("cfg");
    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&cfg_dir).expect("create cfg dir");
    fs::create_dir_all(&bin_dir).expect("create bin dir");

    fs::write(
        cfg_dir.join("rules.toml"),
        r#"
version = 1
[[rules]]
name = "definitely_not_running_proc_123"
limit = 21
domain = "user"
created_at = "2026-03-05T11:00:00+08:00"
updated_at = "2026-03-05T11:00:00+08:00"
"#,
    )
    .expect("write rules");

    let fake_launchctl = bin_dir.join("launchctl");
    fs::write(
        &fake_launchctl,
        "#!/bin/sh\necho 'LEAK_FROM_LAUNCHCTL'\nexit 0\n",
    )
    .expect("write fake launchctl");
    Command::new("chmod")
        .args(["+x", fake_launchctl.to_string_lossy().as_ref()])
        .status()
        .expect("chmod fake launchctl");

    let old_path = std::env::var("PATH").unwrap_or_default();
    let path = format!("{}:{}", bin_dir.display(), old_path);

    let output = Command::new(env!("CARGO_BIN_EXE_cpulimit-top"))
        .arg("watches")
        .env("CPULIMIT_TOP_CONFIG_DIR", &cfg_dir)
        .env("PATH", path)
        .output()
        .expect("run watches");

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("LEAK_FROM_LAUNCHCTL"));
    assert!(stdout.contains("NAME"));
}
