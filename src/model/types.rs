use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

pub const DEFAULT_TRIGGER_CPU: f32 = 25.0;
pub const DEFAULT_RELEASE_CPU: f32 = 8.0;

fn default_trigger_cpu() -> f32 {
    DEFAULT_TRIGGER_CPU
}

fn default_release_cpu() -> f32 {
    DEFAULT_RELEASE_CPU
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Domain {
    User,
    System,
}

impl std::fmt::Display for Domain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::User => write!(f, "user"),
            Self::System => write!(f, "system"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub name: String,
    pub limit: u16,
    #[serde(default = "default_trigger_cpu")]
    pub trigger_cpu: f32,
    #[serde(default = "default_release_cpu")]
    pub release_cpu: f32,
    #[serde(default)]
    pub args_contains: Option<String>,
    pub domain: Domain,
    pub created_at: DateTime<Local>,
    pub updated_at: DateTime<Local>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesFile {
    pub version: u16,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ManagedMode {
    Adhoc,
    Watch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "lowercase")]
pub enum ManagedTarget {
    Pid(u32),
    Name(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedInstance {
    pub id: String,
    pub mode: ManagedMode,
    pub cpulimit_pid: u32,
    pub target: ManagedTarget,
    #[serde(default)]
    pub rule_name: Option<String>,
    #[serde(default)]
    pub limit: Option<u16>,
    #[serde(default)]
    pub last_observed_cpu: Option<f32>,
    pub domain: Domain,
    pub started_at: DateTime<Local>,
    pub owner_label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateFile {
    pub version: u16,
    pub instances: Vec<ManagedInstance>,
}

impl Default for RulesFile {
    fn default() -> Self {
        Self {
            version: 2,
            rules: Vec::new(),
        }
    }
}

impl Default for StateFile {
    fn default() -> Self {
        Self {
            version: 2,
            instances: Vec::new(),
        }
    }
}
