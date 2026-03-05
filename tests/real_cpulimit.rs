use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use tempfile::tempdir;

fn detect_cpulimit() -> Option<PathBuf> {
    let candidates = [
        "/opt/homebrew/bin/cpulimit",
        "/usr/local/bin/cpulimit",
        "cpulimit",
    ];
    for c in candidates {
        let out = Command::new(c).arg("--help").output();
        if let Ok(output) = out {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stdout.contains("Usage: cpulimit") || stderr.contains("Usage: cpulimit") {
                return Some(PathBuf::from(c));
            }
        }
    }
    None
}

#[test]
fn integration_real_cpulimit_once() {
    let Some(cpulimit_bin) = detect_cpulimit() else {
        eprintln!("skip: cpulimit not installed");
        return;
    };

    let mut hog = Command::new("yes")
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn cpu hog");
    let hog_pid = hog.id();

    let dir = tempdir().expect("tempdir");
    let cfg_dir = dir.path().join("cfg");
    fs::create_dir_all(&cfg_dir).expect("create cfg dir");

    let output = Command::new(env!("CARGO_BIN_EXE_cpuguard"))
        .args([
            "top",
            "--once",
            "--pid",
            &hog_pid.to_string(),
            "--limit",
            "20",
        ])
        .env("CPULIMIT_TOP_CONFIG_DIR", &cfg_dir)
        .env("CPULIMIT_TOP_CPULIMIT_BIN", &cpulimit_bin)
        .output()
        .expect("run binary");

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    thread::sleep(Duration::from_millis(400));

    let state_text = fs::read_to_string(cfg_dir.join("state.json")).expect("state exists");
    let parsed: serde_json::Value = serde_json::from_str(&state_text).expect("parse state");
    let cpulimit_pid = parsed["instances"][0]["cpulimit_pid"]
        .as_u64()
        .expect("cpulimit_pid") as u32;

    let alive = Command::new("kill")
        .args(["-0", &cpulimit_pid.to_string()])
        .status()
        .expect("check pid")
        .success();
    assert!(alive, "cpulimit pid should be alive");

    let _ = Command::new("kill")
        .args(["-TERM", &cpulimit_pid.to_string()])
        .status();
    let _ = hog.kill();
    let _ = hog.wait();
}
