use std::env;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub config_dir: PathBuf,
    pub rules_file: PathBuf,
    pub state_file: PathBuf,
    pub launch_agents_dir: PathBuf,
    pub cpulimit_bin: PathBuf,
    pub label_prefix: String,
}

impl AppConfig {
    pub fn load() -> Self {
        let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let config_dir = env::var("CPULIMIT_TOP_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(format!("{home}/.config/cpulimit-top")));
        let launch_agents_dir = env::var("CPULIMIT_TOP_LAUNCH_AGENTS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(format!("{home}/Library/LaunchAgents")));

        let cpulimit_bin = env::var("CPULIMIT_TOP_CPULIMIT_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|_| detect_cpulimit_bin());

        let rules_file = config_dir.join("rules.toml");
        let state_file = config_dir.join("state.json");

        Self {
            config_dir,
            rules_file,
            state_file,
            launch_agents_dir,
            cpulimit_bin,
            label_prefix: "com.cpulimit-top".to_string(),
        }
    }
}

fn detect_cpulimit_bin() -> PathBuf {
    let known = ["/opt/homebrew/bin/cpulimit", "/usr/local/bin/cpulimit"];
    for path in known {
        let p = PathBuf::from(path);
        if p.exists() {
            return p;
        }
    }
    PathBuf::from("cpulimit")
}
