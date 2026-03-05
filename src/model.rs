use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
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
            version: 1,
            rules: Vec::new(),
        }
    }
}

impl Default for StateFile {
    fn default() -> Self {
        Self {
            version: 1,
            instances: Vec::new(),
        }
    }
}
