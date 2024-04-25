use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;

use duct::cmd;
use itertools::Itertools;
use once_cell::sync::OnceCell;
use serde::Deserialize;
use serde::Serialize;
use tokio::process::Command as TokioCommand;

use crate::internal::cache::CacheObject;
use crate::internal::cache::HomebrewInstalled;
use crate::internal::cache::HomebrewOperationCache;
use crate::internal::cache::UpEnvironmentsCache;
use crate::internal::config::up::utils::run_progress;
use crate::internal::config::up::utils::ProgressHandler;
use crate::internal::config::up::utils::RunConfig;
use crate::internal::config::up::utils::UpProgressHandler;
use crate::internal::config::up::UpError;
use crate::internal::config::up::UpOptions;
use crate::internal::config::utils::is_executable;
use crate::internal::config::ConfigValue;
use crate::internal::env::homebrew_prefix;
use crate::internal::user_interface::StringColor;
use crate::internal::workdir;
use crate::omni_warning;

static LOCAL_TAP: &str = "omni/local";
static BREW_UPDATED: OnceCell<bool> = OnceCell::new();

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UpConfigHomebrew {
    #[serde(default = "Vec::new", skip_serializing_if = "Vec::is_empty")]
    pub install: Vec<HomebrewInstall>,
    #[serde(default = "Vec::new", skip_serializing_if = "Vec::is_empty")]
    pub tap: Vec<HomebrewTap>,
}

impl UpConfigHomebrew {
    pub fn from_config_value(config_value: Option<&ConfigValue>) -> Self {
        let install = HomebrewInstall::from_config_value(config_value);
        let tap = HomebrewTap::from_config_value(config_value, &install);

        UpConfigHomebrew { install, tap }
    }

    pub fn up(
        &self,
        options: &UpOptions,
        progress_handler: &UpProgressHandler,
    ) -> Result<(), UpError> {
        progress_handler.init("homebrew:".light_blue());
        progress_handler.progress("installing homebrew dependencies".to_string());

        let num_taps = self.tap.len();
        for (idx, tap) in self.tap.iter().enumerate() {
            let subhandler = progress_handler.subhandler(
                &format!(
                    "[{current:padding$}/{total:padding$}] ",
                    current = idx + 1,
                    total = num_taps,
                    padding = format!("{}", num_taps).len(),
                )
                .light_yellow(),
            );
            tap.up(&subhandler)?;
        }

        let num_installs = self.install.len();
        for (idx, install) in self.install.iter().enumerate() {
            let subhandler = progress_handler.subhandler(
                &format!(
                    "[{current:padding$}/{total:padding$}] ",
                    current = idx + 1,
                    total = num_installs,
                    padding = format!("{}", num_installs).len(),
                )
                .light_yellow(),
            );
            install.up(options, &subhandler)?;
        }

        progress_handler.success_with_message(self.get_up_message());

        Ok(())
    }

    fn get_up_message(&self) -> String {
        let count_taps: HashMap<HomebrewHandled, usize> = self
            .tap
            .iter()
            .map(|tap| tap.handling())
            .fold(HashMap::new(), |mut map, item| {
                *map.entry(item).or_insert(0) += 1;
                map
            });
        let handled_taps: Vec<String> = self
            .tap
            .iter()
            .filter_map(|tap| match tap.handling() {
                HomebrewHandled::Handled | HomebrewHandled::Updated | HomebrewHandled::Noop => {
                    Some(tap.name.clone())
                }
                _ => None,
            })
            .sorted()
            .collect();

        let count_installs: HashMap<HomebrewHandled, usize> = self
            .install
            .iter()
            .map(|install| install.handling())
            .fold(HashMap::new(), |mut map, item| {
                *map.entry(item).or_insert(0) += 1;
                map
            });
        let handled_installs: Vec<String> = self
            .install
            .iter()
            .filter_map(|install| match install.handling() {
                HomebrewHandled::Handled | HomebrewHandled::Updated | HomebrewHandled::Noop => {
                    Some(install.package_id())
                }
                _ => None,
            })
            .sorted()
            .collect();

        let mut messages = vec![];

        for (name, count, handled) in [
            ("tap", count_taps, handled_taps),
            ("formula", count_installs, handled_installs),
        ] {
            let count_handled = handled.len();
            if count_handled == 0 {
                continue;
            }

            let mut numbers = vec![];

            if let Some(count) = count.get(&HomebrewHandled::Handled) {
                numbers.push(format!("+{}", count).green());
            }

            if let Some(count) = count.get(&HomebrewHandled::Updated) {
                numbers.push(format!("^{}", count).yellow());
            }

            if let Some(count) = count.get(&HomebrewHandled::Noop) {
                numbers.push(format!("{}", count));
            }

            if numbers.is_empty() {
                continue;
            }

            messages.push(format!(
                "{} {}{} {}",
                numbers.join(", "),
                name,
                if count_handled > 1 { "s" } else { "" },
                format!("({})", handled.join(", ")).light_black().italic(),
            ));
        }

        if messages.is_empty() {
            "nothing done".to_string()
        } else {
            messages.join(" and ")
        }
    }

    pub fn down(&self, progress_handler: &UpProgressHandler) -> Result<(), UpError> {
        let workdir = workdir(".");
        let wd_id = match workdir.id() {
            Some(wd_id) => wd_id,
            None => return Ok(()),
        };

        if let Err(err) = HomebrewOperationCache::exclusive(|brew_cache| {
            progress_handler.init("homebrew:".light_blue());
            progress_handler.progress("updating homebrew dependencies".to_string());

            let mut updated = false;

            for install in brew_cache
                .installed
                .iter_mut()
                .filter(|install| install.required_by.contains(&wd_id))
            {
                install.required_by.retain(|id| id != &wd_id);
                updated = true;
            }

            for tap in brew_cache
                .tapped
                .iter_mut()
                .filter(|tap| tap.required_by.contains(&wd_id))
            {
                tap.required_by.retain(|id| id != &wd_id);
                updated = true;
            }

            updated
        }) {
            progress_handler.progress(format!("failed to update cache: {}", err).light_yellow());
        }

        progress_handler.success_with_message("homebrew dependencies cleaned".light_green());

        Ok(())
    }

    pub fn cleanup(progress_handler: &UpProgressHandler) -> Result<Option<String>, UpError> {
        let wd = workdir(".");
        let wd_id = match wd.id() {
            Some(wd_id) => wd_id,
            None => return Err(UpError::Exec("failed to get workdir id".to_string())),
        };

        let mut return_value: Result<Option<String>, UpError> = Ok(None);
        let mut updated = false;
        let mut to_untap = vec![];
        let mut to_uninstall = vec![];

        if let Err(err) = HomebrewOperationCache::exclusive(|brew_cache| {
            progress_handler.init("homebrew:".light_blue());
            progress_handler.progress("checking for unused homebrew dependencies".to_string());

            // Cleanup the references to this repository for
            // any installed formulae or casks that is not currently
            // listed in the up configuration
            for install in brew_cache
                .installed
                .iter_mut()
                .filter(|install| install.required_by.contains(&wd_id) && install.stale())
            {
                install.required_by.retain(|id| id != &wd_id);
                updated = true;
            }

            // Cleanup the references to this repository for
            // any tap that is not currently listed in the up
            // configuration
            for tap in brew_cache
                .tapped
                .iter_mut()
                .filter(|tap| tap.required_by.contains(&wd_id) && tap.stale())
            {
                tap.required_by.retain(|id| id != &wd_id);
                updated = true;
            }

            // Get the list of formulae and casks that are not
            // required by any other repository but that have
            // been installed by omni
            to_uninstall = brew_cache
                .installed
                .iter_mut()
                .enumerate()
                .rev() // reverse so we can remove from the end
                .filter_map(|(idx, install)| {
                    if install.installed && install.removable() {
                        Some((idx, HomebrewInstall::from_cache(install)))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();

            let num_uninstalls = to_uninstall.len();
            for (idx, (rmidx, install)) in to_uninstall.iter().enumerate() {
                let subhandler = progress_handler.subhandler(
                    &format!(
                        "[{current:padding$}/{total:padding$}] ",
                        current = idx + 1,
                        total = num_uninstalls,
                        padding = format!("{}", num_uninstalls).len(),
                    )
                    .light_yellow(),
                );

                if let Err(err) = install.down(&subhandler) {
                    progress_handler.error();
                    return_value = Err(err);
                    return updated;
                }

                brew_cache.installed.remove(*rmidx);
                brew_cache.update_cache.removed_homebrew_install(
                    &install.name(),
                    install.version(),
                    install.is_cask(),
                );
                updated = true;
            }

            let current_installed = brew_cache.installed.len();
            brew_cache.installed.retain(|install| !install.removable());
            if current_installed != brew_cache.installed.len() {
                updated = true;
            }

            // Get the list of taps that are not required by any
            // other repository but that have been tapped by omni
            to_untap = brew_cache
                .tapped
                .iter_mut()
                .enumerate()
                .rev() // reverse so we can remove from the end
                .filter_map(|(idx, tap)| {
                    if tap.tapped && tap.removable() {
                        Some((idx, HomebrewTap::from_name(&tap.name)))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();

            let num_untaps = to_untap.len();
            for (idx, (rmidx, tap)) in to_untap.iter().enumerate() {
                let subhandler = progress_handler.subhandler(
                    &format!(
                        "[{current:padding$}/{total:padding$}] ",
                        current = idx + 1,
                        total = num_untaps,
                        padding = format!("{}", num_untaps).len(),
                    )
                    .light_yellow(),
                );

                if let Err(err) = tap.down(&subhandler) {
                    // If the error is that the tap is still in use, we'll consider this a success
                    // and make it so omni does not own the tap installation anymore
                    if err != UpError::HomebrewTapInUse {
                        progress_handler.error();
                        return_value = Err(err);
                        return updated;
                    }
                }

                brew_cache.tapped.remove(*rmidx);
                updated = true;
            }

            let current_tapped = brew_cache.tapped.len();
            brew_cache.tapped.retain(|tap| !tap.removable());
            if current_tapped != brew_cache.tapped.len() {
                updated = true;
            }

            updated
        }) {
            progress_handler.progress(format!("failed to update cache: {}", err).light_yellow());
        }

        let mut messages = Vec::new();

        if updated {
            let handled_taps = to_untap
                .iter()
                .filter(|(_idx, tap)| tap.was_handled())
                .collect::<Vec<_>>();
            let handled_taps_count = handled_taps.len();
            if handled_taps_count > 0 {
                messages.push(format!(
                    "{} tap{} {}",
                    format!("-{}", handled_taps_count).red(),
                    if handled_taps_count > 1 { "s" } else { "" },
                    format!(
                        "({})",
                        handled_taps
                            .iter()
                            .map(|(_idx, tap)| tap.name.clone())
                            .sorted()
                            .join(", ")
                    )
                    .light_black()
                    .italic(),
                ));
            }

            let handled_installs = to_uninstall
                .iter()
                .filter(|(_idx, install)| install.was_handled())
                .collect::<Vec<_>>();
            let handled_installs_count = handled_installs.len();
            if handled_installs_count > 0 {
                messages.push(format!(
                    "{} formula{} {}",
                    format!("-{}", handled_installs_count).red(),
                    if handled_installs_count > 1 { "s" } else { "" },
                    format!(
                        "({})",
                        handled_installs
                            .iter()
                            .map(|(_idx, install)| install.name())
                            .sorted()
                            .join(", ")
                    )
                    .light_black()
                    .italic(),
                ));
            }
        }

        if let Err(err) = return_value {
            if messages.is_empty() {
                Err(err)
            } else {
                Err(UpError::Exec(format!(
                    "{}; {}",
                    messages.join(" and "),
                    err,
                )))
            }
        } else if messages.is_empty() {
            Ok(None)
        } else {
            Ok(Some(messages.join(" and ")))
        }
    }

    pub fn is_available(&self) -> bool {
        which::which("brew").is_ok()
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Eq, PartialEq, Hash)]
pub enum HomebrewHandled {
    Handled,
    Noop,
    Updated,
    Unhandled,
}

#[derive(Debug, Serialize, Deserialize, Clone, Eq, PartialEq)]
pub struct HomebrewTap {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,

    #[serde(skip)]
    was_handled: OnceCell<HomebrewHandled>,
}

impl PartialOrd for HomebrewTap {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HomebrewTap {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.name.cmp(&other.name)
    }
}

impl HomebrewTap {
    fn from_config_value(
        config_value: Option<&ConfigValue>,
        installs: &[HomebrewInstall],
    ) -> Vec<Self> {
        #[allow(clippy::mutable_key_type)]
        let mut taps = BTreeSet::new();

        if let Some(config_value) = config_value {
            if let Some(config_value) = config_value.as_table() {
                if let Some(parsed_taps) = config_value.get("tap") {
                    taps.extend(Self::parse_taps(parsed_taps));
                }
            }
        }

        for install in installs {
            // If the formula name is `a/b/c`, then really the formula
            // name is `c` and the tap is `a/b`. We can use this to
            // force-add the tap to the list of taps if it was not
            // explicitly defined in the configuration
            let split = install.name.split('/').collect::<Vec<_>>();
            if split.len() == 3 {
                let tap_name = format!("{}/{}", split[0], split[1]);
                taps.insert(Self::from_name(&tap_name));
            }
        }

        taps.into_iter().collect()
    }

    fn parse_taps(taps: &ConfigValue) -> Vec<Self> {
        let mut parsed_taps = Vec::new();

        if let Some(taps_array) = taps.as_array() {
            for config_value in taps_array {
                if let Some(tap) = Self::parse_tap(None, &config_value) {
                    parsed_taps.push(tap);
                }
            }
        } else if let Some(taps_hash) = taps.as_table() {
            for (tap_name, config_value) in taps_hash {
                parsed_taps.push(Self::parse_config(tap_name.to_string(), &config_value));
            }
        } else if taps.as_str().is_some() {
            if let Some(tap) = Self::parse_tap(None, taps) {
                parsed_taps.push(tap);
            }
        }

        parsed_taps
    }

    fn parse_tap(name: Option<String>, config_value: &ConfigValue) -> Option<Self> {
        if let Some(name) = name {
            return Some(Self::parse_config(name, config_value));
        }

        if let Some(tap_str) = config_value.as_str() {
            return Some(Self {
                name: tap_str.to_string(),
                url: None,
                was_handled: OnceCell::new(),
            });
        } else if let Some(tap_hash) = config_value.as_table() {
            if let Some(name) = tap_hash.get("repo") {
                if let Some(name) = name.as_str() {
                    return Some(Self::parse_config(name, config_value));
                }
                return None;
            }

            if tap_hash.len() == 1 {
                let (name, config_value) = tap_hash.iter().next().unwrap();
                return Some(Self::parse_config(name.to_string(), config_value));
            }
        }

        None
    }

    fn parse_config(name: String, config_value: &ConfigValue) -> Self {
        let mut url = None;

        if let Some(tap_str) = config_value.as_str() {
            url = Some(tap_str.to_string());
        } else if let Some(config_value) = config_value.as_table() {
            if let Some(url_value) = config_value.get("url") {
                url = Some(url_value.as_str().unwrap().to_string());
            }
        }

        Self {
            name,
            url,
            was_handled: OnceCell::new(),
        }
    }

    fn from_name(name: &str) -> Self {
        Self {
            name: name.to_string(),
            url: None,
            was_handled: OnceCell::new(),
        }
    }

    fn update_cache(&self, progress_handler: &dyn ProgressHandler) {
        let workdir = workdir(".");
        let workdir_id = match workdir.id() {
            Some(wd_id) => wd_id,
            None => return,
        };

        progress_handler.progress("updating cache".to_string());

        if let Err(err) = HomebrewOperationCache::exclusive(|brew_cache| {
            brew_cache.add_tap(&workdir_id, &self.name, self.was_handled())
        }) {
            progress_handler.progress(format!("failed to update cache: {}", err));
        } else {
            progress_handler.progress("updated cache".to_string());
        }
    }

    fn up(&self, progress_handler: &UpProgressHandler) -> Result<(), UpError> {
        let progress_handler = progress_handler
            .subhandler(&format!("{} {}: ", "tap".underline(), self.name).light_yellow());

        // TODO: we should update the tap manually in case people have their homebrew
        //       upgrades disabled.
        if self.is_tapped() {
            self.update_cache(&progress_handler);
            if self.was_handled.set(HomebrewHandled::Noop).is_err() {
                unreachable!("failed to set was_handled (tap: {})", self.name);
            }
            progress_handler.success_with_message("already tapped".light_black());
            return Ok(());
        }

        if let Err(err) = self.tap(&progress_handler, true) {
            progress_handler.error();
            return Err(err);
        }

        self.update_cache(&progress_handler);
        progress_handler.success();

        Ok(())
    }

    fn down(&self, progress_handler: &UpProgressHandler) -> Result<(), UpError> {
        let progress_handler = progress_handler
            .subhandler(&format!("{} {}: ", "untap".underline(), self.name).light_yellow());

        if !self.is_tapped() {
            progress_handler.success_with_message("not currently tapped".light_black());
            return Ok(());
        }

        if cmd!("brew", "list", "--full-name")
            .pipe(cmd!("grep", "-q", format!("^{}/", self.name)))
            .run()
            .is_ok()
        {
            progress_handler.error_with_message("tap is still in use, skipping".light_black());
            return Err(UpError::HomebrewTapInUse);
        }

        if let Err(err) = self.tap(&progress_handler, false) {
            progress_handler.error();
            return Err(err);
        }

        progress_handler.success();

        Ok(())
    }

    fn is_tapped(&self) -> bool {
        let mut brew_tap_list = std::process::Command::new("brew");
        brew_tap_list.arg("tap");
        brew_tap_list.stdout(std::process::Stdio::piped());
        brew_tap_list.stderr(std::process::Stdio::null());

        if let Ok(output) = brew_tap_list.output() {
            if let Ok(output_str) = String::from_utf8(output.stdout) {
                return output_str
                    .lines()
                    .any(|line| line.trim() == self.name.as_str());
            }
        }

        false
    }

    fn tap(&self, progress_handler: &UpProgressHandler, tap: bool) -> Result<(), UpError> {
        let mut brew_tap = TokioCommand::new("brew");
        if tap {
            brew_tap.arg("tap");
        } else {
            brew_tap.arg("untap");
        }
        brew_tap.arg(&self.name);

        if let Some(url) = &self.url {
            brew_tap.arg(url);
        }

        brew_tap.stdout(std::process::Stdio::piped());
        brew_tap.stderr(std::process::Stdio::piped());

        let result = run_progress(&mut brew_tap, Some(progress_handler), RunConfig::default());
        if result.is_ok() && self.was_handled.set(HomebrewHandled::Handled).is_err() {
            unreachable!("failed to set was_handled (tap: {})", self.name);
        }
        result
    }

    fn was_handled(&self) -> bool {
        matches!(self.was_handled.get(), Some(HomebrewHandled::Handled))
    }

    fn handling(&self) -> HomebrewHandled {
        match self.was_handled.get() {
            Some(handled) => handled.clone(),
            None => HomebrewHandled::Unhandled,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub enum HomebrewInstallType {
    Formula,
    Cask,
}

#[derive(Debug, Deserialize, Clone)]
pub struct HomebrewInstall {
    install_type: HomebrewInstallType,
    name: String,
    version: Option<String>,

    #[serde(skip)]
    was_handled: OnceCell<HomebrewHandled>,
}

impl Serialize for HomebrewInstall {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: ::serde::ser::Serializer,
    {
        let mut install = HashMap::new();
        install.insert(
            self.name.clone(),
            self.version.clone().unwrap_or("latest".to_string()),
        );

        if self.install_type == HomebrewInstallType::Cask {
            let mut cask = HashMap::new();
            cask.insert("cask".to_string(), install);
            cask.serialize(serializer)
        } else {
            install.serialize(serializer)
        }
    }
}

impl HomebrewInstall {
    pub fn new_formula(name: &str) -> Self {
        Self {
            install_type: HomebrewInstallType::Formula,
            name: name.to_string(),
            version: None,
            was_handled: OnceCell::new(),
        }
    }

    fn from_config_value(config_value: Option<&ConfigValue>) -> Vec<Self> {
        // TODO: maybe support "alternate" packages with `or`

        let mut installs = Vec::new();

        if let Some(config_value) = config_value {
            if let Some(config_value) = config_value.as_table() {
                if let Some(formulae) = config_value.get("install") {
                    installs.extend(Self::parse_formulae(formulae));
                }
            } else {
                installs.extend(Self::parse_formulae(config_value));
            }
        }

        installs
    }

    fn parse_formulae(formulae: &ConfigValue) -> Vec<Self> {
        let mut installs = Vec::new();

        if let Some(formulae) = formulae.as_array() {
            for formula_config_value in formulae {
                let mut install_type = HomebrewInstallType::Formula;
                let mut version = None;
                let mut name = None;

                if let Some(formula_config) = formula_config_value.as_table() {
                    let mut rest_of_config = formula_config_value.clone();

                    if let Some(formula) = formula_config.get("formula") {
                        name = Some(formula.as_str().unwrap().to_string());
                    } else if let Some(cask) = formula_config.get("cask") {
                        install_type = HomebrewInstallType::Cask;
                        name = Some(cask.as_str().unwrap().to_string());
                    } else if formula_config.len() == 1 {
                        let (key, value) = formula_config.iter().next().unwrap();
                        name = Some(key.to_string());
                        rest_of_config = value.clone();
                    }

                    let parse_version = if rest_of_config.is_str() {
                        Some(rest_of_config)
                    } else {
                        rest_of_config.get("version")
                    };

                    if let Some(parse_version) = parse_version {
                        if let Some(parse_version) = parse_version.as_str() {
                            version = Some(parse_version.to_string());
                        } else if let Some(parse_version) = parse_version.as_integer() {
                            version = Some(parse_version.to_string());
                        } else if let Some(parse_version) = parse_version.as_float() {
                            version = Some(parse_version.to_string());
                        }
                    }
                } else if let Some(formula) = formula_config_value.as_str() {
                    name = Some(formula.to_string());
                }

                if let Some(name) = name {
                    installs.push(Self {
                        install_type,
                        name,
                        version,
                        was_handled: OnceCell::new(),
                    });
                }
            }
        } else if let Some(formula) = formulae.as_str() {
            installs.push(Self {
                install_type: HomebrewInstallType::Formula,
                name: formula.to_string(),
                version: None,
                was_handled: OnceCell::new(),
            });
        }

        installs
    }

    fn from_cache(cached: &HomebrewInstalled) -> Self {
        let install_type = if cached.cask {
            HomebrewInstallType::Cask
        } else {
            HomebrewInstallType::Formula
        };

        Self {
            install_type,
            name: cached.name.clone(),
            version: cached.version.clone(),
            was_handled: OnceCell::new(),
        }
    }

    fn update_cache(&self, options: &UpOptions, progress_handler: &dyn ProgressHandler) {
        let workdir = workdir(".");
        let workdir_id = match workdir.id() {
            Some(wd_id) => wd_id,
            None => return,
        };

        progress_handler.progress("updating cache".to_string());

        if let Err(err) = HomebrewOperationCache::exclusive(|brew_cache| {
            brew_cache.add_install(
                &workdir_id,
                &self.name,
                self.version.clone(),
                self.is_cask(),
                self.was_handled(),
            )
        }) {
            progress_handler.progress(format!("failed to update cache: {}", err));
            return;
        }

        let mut bin_paths = self.bin_paths(options);
        if let Some(bin_path) = self.brew_bin_path(options) {
            bin_paths.push(bin_path);
        }

        if !bin_paths.is_empty() {
            if let Err(err) = UpEnvironmentsCache::exclusive(|up_env| {
                for bin_path in bin_paths {
                    up_env.add_path(&workdir_id, bin_path);
                }
                true
            }) {
                progress_handler.progress(format!("failed to update cache: {}", err));
                return;
            }
        }

        progress_handler.progress("updated cache".to_string());
    }

    fn up(&self, options: &UpOptions, progress_handler: &UpProgressHandler) -> Result<(), UpError> {
        let progress_handler = progress_handler.subhandler(
            &format!(
                "install {}{}: ",
                self.name,
                match &self.version {
                    Some(version) => format!(" ({})", version),
                    None => "".to_string(),
                }
            )
            .light_yellow(),
        );

        let installed = self.is_installed(options);
        if installed && self.version.is_some() {
            if self.was_handled.set(HomebrewHandled::Noop).is_err() {
                unreachable!("failed to set was_handled (install: {})", self.name);
            }
            self.update_cache(options, &progress_handler);
            progress_handler.success_with_message("already installed".light_black());
            return Ok(());
        }

        match self.install(options, Some(&progress_handler), installed) {
            Ok(did_something) => {
                let (was_handled, message) = match (installed, did_something) {
                    (true, true) => (HomebrewHandled::Updated, "updated".light_green()),
                    (true, false) => (HomebrewHandled::Noop, "up to date (cached)".light_black()),
                    (false, _) => (HomebrewHandled::Handled, "installed".light_green()),
                };
                if self.was_handled.set(was_handled).is_err() {
                    unreachable!("failed to set was_handled (install: {})", self.name);
                }
                self.update_cache(options, &progress_handler);
                progress_handler.success_with_message(message);
                Ok(())
            }
            Err(err) => {
                progress_handler.error_with_message(err.to_string());
                Err(err)
            }
        }
    }

    fn down(&self, progress_handler: &UpProgressHandler) -> Result<(), UpError> {
        let progress_handler = progress_handler.subhandler(
            &format!(
                "uninstall {}{}: ",
                self.name,
                match &self.version {
                    Some(version) => format!(" ({})", version),
                    None => "".to_string(),
                }
            )
            .light_yellow(),
        );

        let installed = self.is_installed(&UpOptions::new().cache_disabled());
        if !installed {
            progress_handler.success_with_message("not installed".light_black());
            return Ok(());
        }

        if let Err(err) = self.uninstall(&progress_handler) {
            progress_handler.error_with_message(err.to_string());
            return Err(err);
        }

        if self.is_in_local_tap() {
            let tapped_file = self.tapped_file().unwrap();
            if let Err(err) = std::fs::remove_file(tapped_file) {
                progress_handler.error_with_message(format!(
                    "failed to remove formula from local tap: {}",
                    err
                ));
                return Err(UpError::Exec(
                    "failed to remove formula from local tap".to_string(),
                ));
            }

            if cmd!("brew", "list", "--full-name")
                .pipe(cmd!("grep", "-q", format!("^{}/", LOCAL_TAP)))
                .run()
                .is_err()
            {
                let mut brew_untap = TokioCommand::new("brew");
                brew_untap.arg("untap");
                brew_untap.arg(LOCAL_TAP);
                brew_untap.stdout(std::process::Stdio::piped());
                brew_untap.stderr(std::process::Stdio::piped());

                progress_handler.progress("removing local tap".to_string());

                run_progress(
                    &mut brew_untap,
                    Some(&progress_handler),
                    RunConfig::default(),
                )?;
            }
        }

        progress_handler.success_with_message("uninstalled".light_green());

        Ok(())
    }

    fn package_id(&self) -> String {
        format!(
            "{}{}",
            self.name,
            if let Some(version) = &self.version {
                format!("@{}", version)
            } else {
                "".to_string()
            }
        )
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    fn is_cask(&self) -> bool {
        self.install_type == HomebrewInstallType::Cask
    }

    fn is_installed(&self, options: &UpOptions) -> bool {
        if options.read_cache
            && !HomebrewOperationCache::get().should_check_install(
                &self.name,
                self.version.clone(),
                self.is_cask(),
            )
        {
            return true;
        }

        let mut brew_list = std::process::Command::new("brew");
        brew_list.arg("list");
        if self.is_cask() {
            brew_list.arg("--cask");
        } else {
            brew_list.arg("--formula");
        }
        brew_list.arg(self.package_id());
        brew_list.stdout(std::process::Stdio::null());
        brew_list.stderr(std::process::Stdio::null());

        if let Ok(output) = brew_list.output() {
            if output.status.success() {
                if options.write_cache {
                    // Update the cache
                    if let Err(err) = HomebrewOperationCache::exclusive(|cache| {
                        cache.checked_install(&self.name, self.version.clone(), self.is_cask());
                        true
                    }) {
                        omni_warning!(format!("failed to update cache: {}", err));
                    }
                }
                return true;
            }
        }

        false
    }

    fn brew_bin_path(&self, options: &UpOptions) -> Option<PathBuf> {
        if options.read_cache {
            if let Some(bin_path) = HomebrewOperationCache::get().homebrew_bin_path() {
                if !bin_path.is_empty() {
                    return Some(bin_path.into());
                }
            }
        }

        if let Some(brew_prefix) = homebrew_prefix() {
            let bin_path = PathBuf::from(brew_prefix).join("bin");
            if bin_path.exists() {
                if options.write_cache {
                    // Update the cache
                    _ = HomebrewOperationCache::exclusive(|cache| {
                        cache.set_homebrew_bin_path(bin_path.to_string_lossy().to_string());
                        true
                    });
                }

                return Some(bin_path);
            }
        }

        None
    }

    fn bin_paths_from_cask(&self) -> Vec<PathBuf> {
        if !self.is_cask() {
            return vec![];
        }

        // brew --prefix doesn't work for casks, so we can try to
        // check if there is any bin/ directory in the cask path
        let brew_prefix = match homebrew_prefix() {
            Some(prefix) => prefix,
            None => return vec![],
        };

        // Prepare the prefix path for the cask
        let bin_lookup_prefix = PathBuf::from(brew_prefix)
            .join("Caskroom")
            .join(self.package_id());

        // Generate the glob path we can use to search for the bin directory
        let glob_path = bin_lookup_prefix.join("**").join("bin");
        let glob_path = match glob_path.to_str() {
            Some(glob_path) => glob_path,
            None => return vec![],
        };

        // Search for the bin directory
        let entries = if let Ok(entries) = glob::glob(glob_path) {
            entries
        } else {
            return vec![];
        };

        let mut bin_paths = HashSet::new();
        for path in entries.into_iter().flatten() {
            if !path.is_dir() {
                continue;
            }

            // Get the relative path to the bin directory
            let prefix = format!("{}/", bin_lookup_prefix.to_string_lossy());
            let relpath = match path.strip_prefix(&prefix) {
                Ok(relpath) => relpath,
                Err(_) => continue,
            };

            // Check if any directories are starting with dot,
            // and if so, skip the directory
            if relpath
                .components()
                .any(|comp| comp.as_os_str().to_string_lossy().starts_with('.'))
            {
                continue;
            }

            // Get the canonical path to the bin directory
            let path = match path.canonicalize() {
                Ok(path) => path,
                Err(_) => continue,
            };

            // If the path is already in the set, skip it
            if bin_paths.contains(&path) {
                continue;
            }

            // Check if the directory contains any executable
            // files, i.e. files with the +x flag, and if not,
            // skip the directory
            let mut has_executables = false;
            if let Ok(files) = std::fs::read_dir(&path) {
                for entry in files.flatten() {
                    let filepath = entry.path();
                    let filetype = match entry.file_type() {
                        Ok(filetype) => filetype,
                        Err(_) => continue,
                    };

                    if !filetype.is_file() || !is_executable(&filepath) {
                        continue;
                    }

                    // We want to make sure it's binaries without dots
                    // in the filename, as those are usually not meant to
                    // be in the path
                    let filename = match filepath.file_name() {
                        Some(filename) => filename,
                        None => continue,
                    };
                    if filename.to_string_lossy().contains('.') {
                        continue;
                    }

                    has_executables = true;
                }
            }
            if !has_executables {
                continue;
            }

            bin_paths.insert(path);
        }

        bin_paths.into_iter().collect()
    }

    fn bin_paths_from_formula(&self) -> Vec<PathBuf> {
        if self.is_cask() {
            return vec![];
        }

        let mut brew_list = std::process::Command::new("brew");
        brew_list.arg("--prefix");
        brew_list.arg("--installed");
        brew_list.arg(self.package_id());
        brew_list.stdout(std::process::Stdio::piped());
        brew_list.stderr(std::process::Stdio::null());

        if let Ok(output) = brew_list.output() {
            if output.status.success() {
                let bin_path =
                    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim()).join("bin");
                let bin_path = if bin_path.exists() {
                    bin_path
                } else {
                    PathBuf::from("")
                };

                if !bin_path.to_string_lossy().is_empty() {
                    return vec![bin_path];
                }
            }
        }

        vec![]
    }

    fn bin_paths(&self, options: &UpOptions) -> Vec<PathBuf> {
        if options.read_cache {
            if let Some(bin_paths) = HomebrewOperationCache::get().homebrew_install_bin_paths(
                &self.name,
                self.version.clone(),
                self.is_cask(),
            ) {
                return bin_paths.iter().map(PathBuf::from).collect();
            }
        }

        let bin_paths = if self.is_cask() {
            self.bin_paths_from_cask()
        } else {
            self.bin_paths_from_formula()
        };

        if options.write_cache {
            _ = HomebrewOperationCache::exclusive(|cache| {
                cache.set_homebrew_install_bin_paths(
                    &self.name,
                    self.version.clone(),
                    self.is_cask(),
                    bin_paths
                        .iter()
                        .map(|path| path.to_string_lossy().to_string())
                        .collect(),
                );
                true
            });
        }

        bin_paths
    }

    fn is_in_local_tap(&self) -> bool {
        if self.version.is_none() {
            return false;
        }

        if let Some(tapped_file) = self.tapped_file() {
            let tap_path = LOCAL_TAP.replace('/', "/homebrew-");
            let pkg_type = if self.is_cask() { "Cask" } else { "Formula" };
            let expected_end = format!("{}/{}/{}.rb", tap_path, pkg_type, self.package_id());
            return tapped_file.ends_with(&expected_end);
        }

        false
    }

    fn tapped_file(&self) -> Option<String> {
        let mut brew_list = std::process::Command::new("brew");
        brew_list.arg("formula");
        brew_list.arg(self.package_id());
        brew_list.stdout(std::process::Stdio::piped());
        brew_list.stderr(std::process::Stdio::null());

        if let Ok(output) = brew_list.output() {
            if let Ok(output) = String::from_utf8(output.stdout) {
                let output = output.trim();
                if !output.is_empty() {
                    return Some(output.to_string());
                }
            }
        }

        None
    }

    fn install(
        &self,
        options: &UpOptions,
        progress_handler: Option<&dyn ProgressHandler>,
        installed: bool,
    ) -> Result<bool, UpError> {
        if !installed {
            self.extract_package(options, progress_handler)?;
        } else if options.read_cache
            && !HomebrewOperationCache::get().should_update_install(
                &self.name,
                self.version.clone(),
                self.is_cask(),
            )
        {
            if let Some(progress_handler) = progress_handler {
                progress_handler.progress("already up to date".light_black())
            }

            return Ok(false);
        }

        let mut run_config = RunConfig::default();

        let mut brew_install = TokioCommand::new("brew");
        if installed {
            brew_install.arg("upgrade");
        } else {
            brew_install.arg("install");
        }
        if self.install_type == HomebrewInstallType::Cask {
            brew_install.arg("--cask");
            run_config.with_askpass();
        } else {
            brew_install.arg("--formula");
        }
        brew_install.arg(self.package_id());

        brew_install.stdout(std::process::Stdio::piped());
        brew_install.stderr(std::process::Stdio::piped());

        if let Some(progress_handler) = progress_handler {
            progress_handler.progress(if installed {
                "checking for upgrades".to_string()
            } else {
                "installing".to_string()
            })
        }

        match run_progress(&mut brew_install, progress_handler, run_config) {
            Ok(_) => {
                if options.write_cache {
                    // Update the cache
                    if let Err(err) = HomebrewOperationCache::exclusive(|cache| {
                        cache.updated_install(&self.name, self.version.clone(), self.is_cask());
                        true
                    }) {
                        return Err(UpError::Cache(err.to_string()));
                    }
                }

                Ok(true)
            }
            Err(err) => {
                if options.fail_on_upgrade {
                    Err(err)
                } else {
                    if let Some(progress_handler) = progress_handler {
                        progress_handler.progress(format!("failed to upgrade: {}", err).red())
                    }

                    Ok(false)
                }
            }
        }
    }

    fn uninstall(&self, progress_handler: &UpProgressHandler) -> Result<(), UpError> {
        let mut brew_uninstall = TokioCommand::new("brew");
        brew_uninstall.arg("uninstall");
        if self.install_type == HomebrewInstallType::Cask {
            brew_uninstall.arg("--cask");
        } else {
            brew_uninstall.arg("--formula");
        }
        brew_uninstall.arg(self.package_id());

        brew_uninstall.stdout(std::process::Stdio::piped());
        brew_uninstall.stderr(std::process::Stdio::piped());

        progress_handler.progress("uninstalling".to_string());

        let result = run_progress(
            &mut brew_uninstall,
            Some(progress_handler),
            RunConfig::default(),
        );
        if result.is_ok() && self.was_handled.set(HomebrewHandled::Handled).is_err() {
            unreachable!("failed to set was_handled (install: {})", self.name);
        }
        result
    }

    fn extract_package(
        &self,
        options: &UpOptions,
        progress_handler: Option<&dyn ProgressHandler>,
    ) -> Result<(), UpError> {
        if self.version.is_none() {
            return Ok(());
        }

        if let Some(progress_handler) = progress_handler {
            progress_handler.progress("checking for formula".to_string())
        }

        let mut brew_info = std::process::Command::new("brew");
        brew_info.arg("info");
        brew_info.arg(self.package_id());
        brew_info.stdout(std::process::Stdio::null());
        brew_info.stderr(std::process::Stdio::null());

        if let Ok(output) = brew_info.output() {
            if output.status.success() {
                if let Some(progress_handler) = progress_handler {
                    progress_handler.progress("formula available".to_string())
                }
                return Ok(());
            }
        }

        if let Some(progress_handler) = progress_handler {
            progress_handler.progress("checking for local tap".to_string())
        }

        let local_tap_exists = cmd!("brew", "tap")
            .pipe(cmd!("grep", "-q", LOCAL_TAP))
            .run();
        if local_tap_exists.is_err() {
            let mut brew_tap_new = TokioCommand::new("brew");
            brew_tap_new.arg("tap-new");
            brew_tap_new.arg("--no-git");
            brew_tap_new.arg(LOCAL_TAP);
            brew_tap_new.stdout(std::process::Stdio::piped());
            brew_tap_new.stderr(std::process::Stdio::piped());

            if let Some(progress_handler) = progress_handler {
                progress_handler.progress("creating local tap".to_string())
            }

            run_progress(&mut brew_tap_new, progress_handler, RunConfig::default())?;
        }
        // else {
        // let mut brew_tap_formula_exists = std::process::Command::new("brew");
        // brew_tap_formula_exists.arg("formula");
        // brew_tap_formula_exists.arg(self.package_id());
        // brew_tap_formula_exists.stdout(std::process::Stdio::piped());
        // brew_tap_formula_exists.stderr(std::process::Stdio::null());

        // progress_handler.clone().map(|progress_handler| {
        // progress_handler.progress("checking for formula".to_string())
        // });

        // // Check if the output of the command is non-empty
        // if let Ok(output) = brew_tap_formula_exists.output() {
        // if output.status.success() {
        // progress_handler.clone().map(|progress_handler| {
        // progress_handler.progress("formula found, no need to extract".to_string())
        // });
        // return Ok(());
        // }
        // }
        // }

        if !options.read_cache || HomebrewOperationCache::get().should_update_homebrew() {
            let mut update_brew_cache = false;
            let brew_updated = BREW_UPDATED.get_or_init(|| {
                if let Some(progress_handler) = progress_handler {
                    progress_handler.progress("updating homebrew".to_string())
                }

                let mut brew_update = TokioCommand::new("brew");
                brew_update.arg("update");
                brew_update.env("HOMEBREW_NO_INSTALL_FROM_API", "1");
                brew_update.stdout(std::process::Stdio::piped());
                brew_update.stderr(std::process::Stdio::piped());

                let result = run_progress(&mut brew_update, progress_handler, RunConfig::default());
                if result.is_err() {
                    return false;
                }

                update_brew_cache = true;

                true
            });
            if !brew_updated {
                return Err(UpError::Exec("failed to update homebrew".to_string()));
            }

            if options.write_cache && update_brew_cache {
                // Update the cache
                if let Err(err) = HomebrewOperationCache::exclusive(|cache| {
                    cache.updated_homebrew();
                    true
                }) {
                    return Err(UpError::Cache(err.to_string()));
                }
            }
        }

        let mut brew_extract = TokioCommand::new("brew");
        brew_extract.arg("extract");
        brew_extract.arg("--version");
        brew_extract.arg(self.version.as_ref().unwrap());
        brew_extract.arg(&self.name);
        brew_extract.arg(LOCAL_TAP);
        brew_extract.stdout(std::process::Stdio::piped());
        brew_extract.stderr(std::process::Stdio::piped());

        if let Some(progress_handler) = progress_handler {
            progress_handler.progress("extracting package".to_string())
        }

        run_progress(&mut brew_extract, progress_handler, RunConfig::default())
    }

    fn was_handled(&self) -> bool {
        matches!(self.was_handled.get(), Some(HomebrewHandled::Handled))
    }

    fn handling(&self) -> HomebrewHandled {
        match self.was_handled.get() {
            Some(handled) => handled.clone(),
            None => HomebrewHandled::Unhandled,
        }
    }
}
