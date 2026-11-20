//! Process startup shared by the app and `flint mcp`: config directory, settings, `PATH`, and the
//! kubeconfig file list.

use std::path::PathBuf;

use tracing_subscriber::EnvFilter;

use crate::env_path::{self, PathSource, ResolvedPath};
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
            watch_timing: Default::default(),
        }
    }

    /// The kubeconfig files for the settings loaded at startup.
    pub fn kubeconfig_paths(&self) -> Vec<PathBuf> {
        kubeconfig::paths_for_settings(&self.kubeconfig_base, &self.settings, self.home.as_deref())
    }

    /// Logs where `PATH`, the config directory and the settings came from. Call it once logging
    /// is set up ([`init_logging`]).
    pub fn log_summary(&self) {
        match &self.path.source {
            PathSource::Settings => tracing::info!("PATH from settings.toml"),
            PathSource::LoginShell => tracing::info!("PATH from login shell"),
            PathSource::Fallback(reason) => {
                tracing::warn!(reason, "PATH from fallback (login shell not used)")
            }
        }
        tracing::debug!(path = %self.path.value, "resolved PATH");
        tracing::info!(config_dir = %self.config_dir.display(), "config directory");
        if let Some(e) = &self.settings_error {
            tracing::warn!("using default settings: {e}");
        }
    }
}

/// Sends logs to stderr, filtered by [`log_filter`] from `FLINT_LOG` and `RUST_LOG`. Never
/// stdout: in `flint mcp`, stdout carries the protocol. Call it once, after [`bootstrap`].
/// `verbose` (`--verbose`) raises the default from warnings to info.
pub fn init_logging(verbose: bool) {
    let flint_log = std::env::var("FLINT_LOG").ok();
    let rust_log = std::env::var("RUST_LOG").ok();
    tracing_subscriber::fmt()
        .with_env_filter(log_filter(
            flint_log.as_deref(),
            rust_log.as_deref(),
            verbose,
        ))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
}

/// The log filter: `FLINT_LOG` if set, else `RUST_LOG`, else `info` when `verbose` and `warn`
/// otherwise, so a plain run prints only warnings and errors. An empty or unparsable variable
/// counts as unset. A variable wins over `verbose`: it says exactly what to log.
pub fn log_filter(flint_log: Option<&str>, rust_log: Option<&str>, verbose: bool) -> EnvFilter {
    [flint_log, rust_log]
        .into_iter()
        .flatten()
        .filter(|directives| !directives.trim().is_empty())
        .find_map(|directives| EnvFilter::try_new(directives).ok())
        .unwrap_or_else(|| EnvFilter::new(if verbose { "info" } else { "warn" }))
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

#[cfg(test)]
mod tests {
    use tracing_subscriber::filter::LevelFilter;

    use super::*;

    fn boot(extra: Vec<PathBuf>) -> Bootstrap {
        Bootstrap {
            config_dir: PathBuf::from("/cfg"),
            settings: Settings {
                kubeconfig_paths: extra,
                ..Default::default()
            },
            settings_error: Some("bad".into()),
            path: ResolvedPath {
                value: "/bin".into(),
                source: PathSource::Settings,
            },
            kubeconfig_base: vec![PathBuf::from("/home/u/.kube/config")],
            home: Some(PathBuf::from("/home/u")),
        }
    }

    #[test]
    fn the_core_watches_files_and_gets_the_startup_settings() {
        let config = boot(Vec::new()).core_config();
        assert!(config.watch_files);
        assert_eq!(config.config_dir, PathBuf::from("/cfg"));
        assert_eq!(config.settings_error.as_deref(), Some("bad"));
        assert_eq!(
            config.kubeconfig_base,
            [PathBuf::from("/home/u/.kube/config")]
        );
    }

    fn max_level(flint_log: Option<&str>, rust_log: Option<&str>, verbose: bool) -> LevelFilter {
        log_filter(flint_log, rust_log, verbose)
            .max_level_hint()
            .unwrap()
    }

    #[test]
    fn plain_runs_log_warnings_and_verbose_runs_info() {
        assert_eq!(max_level(None, None, false), LevelFilter::WARN);
        assert_eq!(max_level(None, None, true), LevelFilter::INFO);
    }

    #[test]
    fn flint_log_then_rust_log_override_the_default_and_verbose() {
        assert_eq!(max_level(Some("debug"), None, false), LevelFilter::DEBUG);
        assert_eq!(max_level(Some("error"), None, true), LevelFilter::ERROR);
        assert_eq!(max_level(None, Some("trace"), false), LevelFilter::TRACE);
        assert_eq!(
            max_level(Some("debug"), Some("trace"), false),
            LevelFilter::DEBUG
        );
        assert_eq!(
            log_filter(Some("flint_core=debug"), None, false).to_string(),
            "flint_core=debug"
        );
    }

    #[test]
    fn empty_or_unparsable_filters_count_as_unset() {
        assert_eq!(max_level(Some(""), None, false), LevelFilter::WARN);
        assert_eq!(
            max_level(Some("  "), Some("debug"), false),
            LevelFilter::DEBUG
        );
        assert_eq!(max_level(Some("=bogus=["), None, true), LevelFilter::INFO);
        assert_eq!(
            max_level(Some("=bogus=["), Some("error"), true),
            LevelFilter::ERROR
        );
    }

    #[test]
    fn the_summary_names_the_path_source_and_a_settings_error() {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let writer = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || Writer(writer.clone()))
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || boot(Vec::new()).log_summary());
        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(text.contains("PATH from settings.toml"), "{text}");
        assert!(text.contains("config_dir=/cfg"), "{text}");
        assert!(text.contains("using default settings: bad"), "{text}");
    }

    struct Writer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Writer {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn extra_kubeconfig_paths_follow_the_base_files() {
        let paths = boot(vec![PathBuf::from("~/extra.yaml")]).kubeconfig_paths();
        assert_eq!(
            paths,
            [
                PathBuf::from("/home/u/.kube/config"),
                PathBuf::from("/home/u/extra.yaml")
            ]
        );
    }
}
