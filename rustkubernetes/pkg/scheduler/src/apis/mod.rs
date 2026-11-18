//! Port of `pkg/scheduler/apis/config`: `KubeSchedulerConfiguration` and
//! scheduling profiles.
//!
//! ```yaml
//! apiVersion: kubescheduler.config.k8s.io/v1
//! kind: KubeSchedulerConfiguration
//! profiles:
//! - schedulerName: default-scheduler
//! - schedulerName: bin-packing
//!   plugins:
//!     score:
//!       disabled: [{name: NodeResourcesBalancedAllocation}]
//!       enabled: [{name: NodeResourcesFit, weight: 5}]
//!   pluginConfig:
//!   - name: NodeResourcesFit
//!     args: {scoringStrategy: {type: MostAllocated}}
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use crate::framework::plugins::{new_plugin, DEFAULT_PLUGINS};
use crate::framework::Plugin;

pub const DEFAULT_SCHEDULER_NAME: &str = "default-scheduler";

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KubeSchedulerConfiguration {
    #[serde(default)]
    pub profiles: Vec<ProfileConfig>,
    pub percentage_of_nodes_to_score: Option<i32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileConfig {
    pub scheduler_name: Option<String>,
    #[serde(default)]
    pub plugins: HashMap<String, PluginSet>,
    #[serde(default)]
    pub plugin_config: Vec<PluginConfig>,
}

#[derive(Debug, Default, Deserialize)]
pub struct PluginSet {
    #[serde(default)]
    pub enabled: Vec<PluginRef>,
    #[serde(default)]
    pub disabled: Vec<PluginRef>,
}

#[derive(Debug, Deserialize)]
pub struct PluginRef {
    pub name: String,
    pub weight: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct PluginConfig {
    pub name: String,
    pub args: Option<Value>,
}

/// A configured framework instance for one `schedulerName`.
pub struct Profile {
    pub scheduler_name: String,
    /// Plugins running PreEnqueue/PreFilter/Filter (all enabled plugins).
    pub plugins: Vec<Arc<dyn Plugin>>,
    /// Scoring plugins with weights.
    pub scorers: Vec<(Arc<dyn Plugin>, i64)>,
}

impl Profile {
    pub fn default_profile() -> Self {
        build_profile(&ProfileConfig::default()).expect("default profile")
    }
}

fn disabled(set: Option<&PluginSet>, name: &str) -> bool {
    set.is_some_and(|s| s.disabled.iter().any(|d| d.name == name || d.name == "*"))
}

/// Applies `plugins`/`pluginConfig` on top of the default plugin set.
pub fn build_profile(cfg: &ProfileConfig) -> Result<Profile, String> {
    let args: HashMap<&str, &Value> = cfg.plugin_config.iter().filter_map(|p| p.args.as_ref().map(|a| (p.name.as_str(), a))).collect();
    let multi = cfg.plugins.get("multiPoint");
    let filter = cfg.plugins.get("filter");
    let score = cfg.plugins.get("score");
    let mut plugins: Vec<Arc<dyn Plugin>> = vec![];
    let mut scorers: Vec<(Arc<dyn Plugin>, i64)> = vec![];
    #[allow(clippy::type_complexity)]
    let add = |plugins: &mut Vec<Arc<dyn Plugin>>,
               scorers: &mut Vec<(Arc<dyn Plugin>, i64)>,
               name: &str,
               weight: i64,
               filter_on: bool,
               score_on: bool|
     -> Result<(), String> {
        let p = new_plugin(name, args.get(name).copied()).ok_or_else(|| format!("plugin {name:?} does not exist"))?;
        if filter_on && !plugins.iter().any(|x| x.name() == name) {
            plugins.push(p.clone());
        }
        if score_on && weight > 0 {
            // Re-enabling with a weight overrides the default weight.
            scorers.retain(|(x, _)| x.name() != name);
            scorers.push((p, weight));
        }
        Ok(())
    };
    for (name, weight) in DEFAULT_PLUGINS {
        if disabled(multi, name) {
            continue;
        }
        add(&mut plugins, &mut scorers, name, *weight, !disabled(filter, name), !disabled(score, name))?;
    }
    for set in [multi, filter, score].into_iter().flatten() {
        for e in &set.enabled {
            let default_w = DEFAULT_PLUGINS.iter().find(|(n, _)| *n == e.name).map(|(_, w)| *w).unwrap_or(1).max(1);
            add(&mut plugins, &mut scorers, &e.name, e.weight.unwrap_or(default_w), true, true)?;
        }
    }
    Ok(Profile { scheduler_name: cfg.scheduler_name.clone().unwrap_or_else(|| DEFAULT_SCHEDULER_NAME.into()), plugins, scorers })
}

/// Parses a config file into profiles (default profile if none are listed).
pub fn load_profiles(yaml: Option<&str>) -> Result<Vec<Profile>, String> {
    let cfg: KubeSchedulerConfiguration = match yaml {
        Some(y) => serde_yaml::from_str(y).map_err(|e| format!("invalid KubeSchedulerConfiguration: {e}"))?,
        None => KubeSchedulerConfiguration::default(),
    };
    if cfg.profiles.is_empty() {
        return Ok(vec![Profile::default_profile()]);
    }
    cfg.profiles.iter().map(build_profile).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_from_yaml() {
        let y = r#"
apiVersion: kubescheduler.config.k8s.io/v1
kind: KubeSchedulerConfiguration
profiles:
- schedulerName: default-scheduler
- schedulerName: packer
  plugins:
    score:
      disabled: [{name: NodeResourcesBalancedAllocation}]
      enabled: [{name: NodeResourcesFit, weight: 5}]
  pluginConfig:
  - name: NodeResourcesFit
    args: {scoringStrategy: {type: MostAllocated}}
"#;
        let p = load_profiles(Some(y)).unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[1].scheduler_name, "packer");
        assert!(!p[1].scorers.iter().any(|(s, _)| s.name() == "NodeResourcesBalancedAllocation"));
        assert_eq!(p[1].scorers.iter().find(|(s, _)| s.name() == "NodeResourcesFit").unwrap().1, 5);
        assert!(p[0].plugins.len() >= 10);
        assert!(load_profiles(Some("profiles: [{plugins: {filter: {enabled: [{name: Nope}]}}}]")).is_err());
    }
}
