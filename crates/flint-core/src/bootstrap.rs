//! Process startup shared by the app and `flint mcp`: config directory, settings, `PATH`, and the
//! kubeconfig file list.

use std::path::PathBuf;

use crate::env_path::{self, ResolvedPath};
use crate::handle::CoreConfig;
use crate::kubeconfig;
use crate::settings::{self, Settings, SettingsError};

#[derive(Debug, Clone)]
pub struct Bootstrap {
    pub config_dir: PathBuf,
    pub settings: Settings,
    /// Set when `settings.toml` exists but could not be read; defaults are used instead.
    pub settings_error: Option<String>,
    pub path: ResolvedPath,
    /// Kubeconfig files from `KUBECONFIG` or `~/.kube/config`, before the settings' extra paths.
    pub kubeconfig_base: Vec<PathBuf>,
    pub home: Option<PathBuf>,
}

impl Bootstrap {
    pub fn core_config(&self) -> CoreConfig {
        CoreConfig {
            config_dir: self.config_dir.clone(),
            settings: self.settings.clone(),
            settings_error: self.settings_error.clone(),
            kubeconfig_base: self.kubeconfig_base.clone(),
            home: self.home.clone(),
            watch_files: true,
        }
    }

    /// The kubeconfig files for the settings loaded at startup.
    pub fn kubeconfig_paths(&self) -> Vec<PathBuf> {
        kubeconfig::paths_for_settings(&self.kubeconfig_base, &self.settings, self.home.as_deref())
    }
}

/// Loads settings, resolves and installs `PATH`, and computes the kubeconfig files.
///
/// # Safety
///
/// Must run while the process is single-threaded, before any runtime or UI starts, because it
/// changes the process environment. See [`env_path::apply_process_path`].
pub unsafe fn bootstrap() -> Result<Bootstrap, SettingsError> {
    let config_dir = settings::config_dir()?;
    let (settings, settings_error) = match settings::load_settings(&config_dir) {
        Ok(s) => (s, None),
        Err(e) => (Settings::default(), Some(e.to_string())),
    };
    // SAFETY: forwarded to the caller.
    let path = unsafe { env_path::apply_process_path(settings.path.as_deref()) };
    Ok(Bootstrap {
        config_dir,
        settings,
        settings_error,
        path,
        kubeconfig_base: kubeconfig::default_base_paths(),
        home: std::env::home_dir(),
    })
}
