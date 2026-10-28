//! A distribution is one buildable harness: a name, the plugins it knows, and
//! the profiles that list them.
//!
//! Melchior's own binary is `Distribution::melchior()`. A product that embeds
//! Melchior depends on this crate, starts from the same distribution, adds
//! its own plugins and profiles, and hands the result to [`crate::cli::main`]:
//!
//! ```no_run
//! use std::sync::Arc;
//! use melchior::cordis::loader::Entry;
//! # struct MyPlugin;
//! # impl melchior::cordis::Plugin for MyPlugin {
//! #     fn name(&self) -> &'static str { "my-plugin" }
//! #     fn apply(&self, _: &melchior::cordis::Context) -> Result<(), String> { Ok(()) }
//! # }
//!
//! fn main() -> std::process::ExitCode {
//!     let mut dist = melchior::Distribution::melchior().named("my-harness", "1.0.0");
//!     dist.registry.register(Arc::new(MyPlugin));
//!     let mut entries = dist.profile("default").unwrap();
//!     entries.insert(0, Entry::new("my-plugin"));
//!     dist.set_profile("default", entries);
//!     melchior::cli::main(dist)
//! }
//! ```

use std::path::PathBuf;

use crate::cordis::loader::{Entry, PluginRegistry};
use crate::plugins;

pub struct Distribution {
    /// Reported as `serverInfo.name` and used as the prefix of CLI messages.
    pub name: String,
    pub version: String,
    pub registry: PluginRegistry,
    profiles: Vec<(String, Vec<Entry>)>,
    /// Profile used when the command line names none.
    pub default_profile: String,
    /// Where session logs live, relative to the harness home.
    pub sessions_dir: PathBuf,
    /// Directory under `$HOME` used by headless runs when no home is set.
    pub home_dir_name: String,
}

impl Distribution {
    /// Melchior as this crate builds it.
    pub fn melchior() -> Distribution {
        Distribution {
            name: "melchior".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            registry: plugins::registry(),
            profiles: plugins::profiles(),
            default_profile: "default".into(),
            sessions_dir: PathBuf::from("melchior").join("sessions"),
            home_dir_name: ".melchior".into(),
        }
    }

    /// Rename the distribution (its CLI messages and `serverInfo`).
    pub fn named(mut self, name: &str, version: &str) -> Distribution {
        self.name = name.into();
        self.version = version.into();
        self
    }

    /// A copy of a profile's entries.
    pub fn profile(&self, name: &str) -> Result<Vec<Entry>, String> {
        self.profiles.iter().find(|(n, _)| n == name).map(|(_, entries)| entries.clone()).ok_or_else(|| {
            let known: Vec<&str> = self.profiles.iter().map(|(n, _)| n.as_str()).collect();
            format!("unknown profile {name:?} (available: {})", known.join(", "))
        })
    }

    /// Add a profile, or replace one with the same name.
    pub fn set_profile(&mut self, name: &str, entries: Vec<Entry>) {
        match self.profiles.iter_mut().find(|(n, _)| n == name) {
            Some((_, existing)) => *existing = entries,
            None => self.profiles.push((name.to_string(), entries)),
        }
    }

    pub fn profile_names(&self) -> Vec<String> {
        self.profiles.iter().map(|(n, _)| n.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_can_be_replaced_and_added() {
        let mut dist = Distribution::melchior();
        let default = dist.profile("default").unwrap();
        assert!(default.iter().any(|e| e.id == "leviathan"));
        assert_eq!(dist.profile("sdk").unwrap().len(), default.len());
        dist.set_profile("sdk", vec![Entry::new("agent-loop")]);
        assert_eq!(dist.profile("sdk").unwrap().len(), 1);
        dist.set_profile("custom", vec![]);
        assert!(dist.profile_names().contains(&"custom".to_string()));
        assert!(dist.profile("nope").unwrap_err().contains("available"));
    }
}
