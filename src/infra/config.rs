use std::env;
use std::path::PathBuf;

use crate::model::Domain;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub config_dir: PathBuf,
    pub rules_file: PathBuf,
    pub state_file: PathBuf,
    pub launch_agents_dir: PathBuf,
    pub launch_daemons_dir: PathBuf,
    pub cpulimit_bin: PathBuf,
    pub label_prefix: String,
}

impl AppConfig {
    pub fn load(domain: Domain) -> Self {
        let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let config_dir = env::var("CPULIMIT_TOP_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| default_config_dir(domain, &home));
        let launch_agents_dir = env::var("CPULIMIT_TOP_LAUNCH_AGENTS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(format!("{home}/Library/LaunchAgents")));
        let launch_daemons_dir = env::var("CPULIMIT_TOP_LAUNCH_DAEMONS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/Library/LaunchDaemons"));

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
            launch_daemons_dir,
            cpulimit_bin,
            label_prefix: "com.cpuguard".to_string(),
        }
    }
}

fn default_config_dir(domain: Domain, home: &str) -> PathBuf {
    match domain {
        Domain::User => PathBuf::from(format!("{home}/.config/cpuguard")),
        Domain::System => PathBuf::from("/Library/Application Support/cpuguard"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_dir_uses_system_wide_path_for_system_domain() {
        assert_eq!(
            default_config_dir(Domain::System, "/Users/demo"),
            PathBuf::from("/Library/Application Support/cpuguard")
        );
    }

    #[test]
    fn default_config_dir_uses_home_path_for_user_domain() {
        assert_eq!(
            default_config_dir(Domain::User, "/Users/demo"),
            PathBuf::from("/Users/demo/.config/cpuguard")
        );
    }
}
