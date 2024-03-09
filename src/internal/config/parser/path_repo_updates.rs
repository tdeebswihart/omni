use std::collections::HashMap;

use serde::Deserialize;
use serde::Serialize;

use crate::internal::config::utils::parse_duration_or_default;
use crate::internal::config::ConfigValue;
use crate::internal::env::shell_is_interactive;
use crate::internal::git::update_git_repo;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PathRepoUpdatesConfig {
    pub enabled: bool,
    pub self_update: PathRepoUpdatesSelfUpdateEnum,
    pub pre_auth: bool,
    pub pre_auth_timeout: u64,
    pub background_updates: bool,
    pub background_updates_timeout: u64,
    pub interval: u64,
    pub ref_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_match: Option<String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub per_repo_config: HashMap<String, PathRepoUpdatesPerRepoConfig>,
}

impl Default for PathRepoUpdatesConfig {
    fn default() -> Self {
        Self {
            enabled: Self::DEFAULT_ENABLED,
            self_update: Self::DEFAULT_SELF_UPDATE,
            pre_auth: Self::DEFAULT_PRE_AUTH,
            pre_auth_timeout: Self::DEFAULT_PRE_AUTH_TIMEOUT,
            background_updates: Self::DEFAULT_BACKGROUND_UPDATES,
            background_updates_timeout: Self::DEFAULT_BACKGROUND_UPDATES_TIMEOUT,
            interval: Self::DEFAULT_INTERVAL,
            ref_type: Self::DEFAULT_REF_TYPE.to_string(),
            ref_match: None,
            per_repo_config: HashMap::new(),
        }
    }
}

impl PathRepoUpdatesConfig {
    const DEFAULT_ENABLED: bool = true;
    const DEFAULT_SELF_UPDATE: PathRepoUpdatesSelfUpdateEnum = PathRepoUpdatesSelfUpdateEnum::Ask;
    const DEFAULT_PRE_AUTH: bool = true;
    const DEFAULT_PRE_AUTH_TIMEOUT: u64 = 120; // 2 minutes
    const DEFAULT_BACKGROUND_UPDATES: bool = true;
    const DEFAULT_BACKGROUND_UPDATES_TIMEOUT: u64 = 3600; // 1 hour
    const DEFAULT_INTERVAL: u64 = 43200; // 12 hours
    const DEFAULT_REF_TYPE: &'static str = "branch";

    pub(super) fn from_config_value(config_value: Option<ConfigValue>) -> Self {
        let config_value = match config_value {
            Some(config_value) => config_value,
            None => return Self::default(),
        };

        let mut per_repo_config = HashMap::new();
        if let Some(value) = config_value.get("per_repo_config") {
            for (key, value) in value.as_table().unwrap() {
                per_repo_config.insert(
                    key.to_string(),
                    PathRepoUpdatesPerRepoConfig::from_config_value(&value),
                );
            }
        };

        let pre_auth_timeout = parse_duration_or_default(
            config_value.get("pre_auth_timeout").as_ref(),
            Self::DEFAULT_PRE_AUTH_TIMEOUT,
        );
        let background_updates_timeout = parse_duration_or_default(
            config_value.get("background_updates_timeout").as_ref(),
            Self::DEFAULT_BACKGROUND_UPDATES_TIMEOUT,
        );
        let interval = parse_duration_or_default(
            config_value.get("interval").as_ref(),
            Self::DEFAULT_INTERVAL,
        );

        let self_update = if let Some(value) = config_value.get_as_bool("self_update") {
            match value {
                true => PathRepoUpdatesSelfUpdateEnum::True,
                false => PathRepoUpdatesSelfUpdateEnum::False,
            }
        } else if let Some(value) = config_value.get_as_str("self_update") {
            match value.to_lowercase().as_str() {
                "true" | "yes" | "y" => PathRepoUpdatesSelfUpdateEnum::True,
                "false" | "no" | "n" => PathRepoUpdatesSelfUpdateEnum::False,
                "nocheck" => PathRepoUpdatesSelfUpdateEnum::NoCheck,
                "ask" => PathRepoUpdatesSelfUpdateEnum::Ask,
                _ => PathRepoUpdatesSelfUpdateEnum::Ask,
            }
        } else if let Some(value) = config_value.get_as_integer("self_update") {
            match value {
                0 => PathRepoUpdatesSelfUpdateEnum::False,
                1 => PathRepoUpdatesSelfUpdateEnum::True,
                _ => PathRepoUpdatesSelfUpdateEnum::Ask,
            }
        } else {
            PathRepoUpdatesSelfUpdateEnum::Ask
        };

        Self {
            enabled: config_value
                .get_as_bool("enabled")
                .unwrap_or(Self::DEFAULT_ENABLED),
            self_update,
            pre_auth: config_value
                .get_as_bool("pre_auth")
                .unwrap_or(Self::DEFAULT_PRE_AUTH),
            pre_auth_timeout,
            background_updates: config_value
                .get_as_bool("background_updates")
                .unwrap_or(Self::DEFAULT_BACKGROUND_UPDATES),
            background_updates_timeout,
            interval,
            ref_type: config_value
                .get_as_str("ref_type")
                .unwrap_or(Self::DEFAULT_REF_TYPE.to_string()),
            ref_match: config_value.get_as_str("ref_match"),
            per_repo_config,
        }
    }

    pub fn update_config(&self, repo_id: &str) -> (bool, String, Option<String>) {
        match self.per_repo_config.get(repo_id) {
            Some(value) => (
                value.enabled,
                value.ref_type.clone(),
                value.ref_match.clone(),
            ),
            None => (self.enabled, self.ref_type.clone(), self.ref_match.clone()),
        }
    }

    pub fn update(&self, repo_id: &str) -> bool {
        let (enabled, ref_type, ref_match) = self.update_config(repo_id);

        if !enabled {
            return false;
        }

        update_git_repo(repo_id, ref_type, ref_match, None, None).unwrap_or(false)
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub enum PathRepoUpdatesSelfUpdateEnum {
    #[serde(rename = "true")]
    True,
    #[serde(rename = "false")]
    False,
    #[serde(rename = "nocheck")]
    NoCheck,
    #[serde(other, rename = "ask")]
    Ask,
}

impl PathRepoUpdatesSelfUpdateEnum {
    pub fn do_not_check(&self) -> bool {
        matches!(self, PathRepoUpdatesSelfUpdateEnum::NoCheck)
    }

    pub fn is_false(&self) -> bool {
        match self {
            PathRepoUpdatesSelfUpdateEnum::False => true,
            PathRepoUpdatesSelfUpdateEnum::Ask => !shell_is_interactive(),
            _ => false,
        }
    }

    pub fn is_ask(&self) -> bool {
        match self {
            PathRepoUpdatesSelfUpdateEnum::Ask => shell_is_interactive(),
            _ => false,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PathRepoUpdatesPerRepoConfig {
    pub enabled: bool,
    pub ref_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_match: Option<String>,
}

impl PathRepoUpdatesPerRepoConfig {
    pub(super) fn from_config_value(config_value: &ConfigValue) -> Self {
        Self {
            enabled: match config_value.get("enabled") {
                Some(value) => value.as_bool().unwrap(),
                None => true,
            },
            ref_type: match config_value.get("ref_type") {
                Some(value) => value.as_str().unwrap().to_string(),
                None => "branch".to_string(),
            },
            ref_match: config_value
                .get("ref_match")
                .map(|value| value.as_str().unwrap().to_string()),
        }
    }
}
