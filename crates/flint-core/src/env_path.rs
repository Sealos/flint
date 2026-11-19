//! Resolving the process `PATH` at startup.
//!
//! Apps started from Finder inherit launchd's minimal `PATH`, so exec auth plugins (`aws`,
//! `gke-gcloud-auth-plugin`, `kubelogin`, ...) and `helm` would not be found. Flint runs the
//! user's shell once as an interactive login shell, reads its `PATH`, and installs it as the
//! process `PATH` before any other thread starts. See ARCHITECTURE.md, "Clusters".

use std::io::Read;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// How long the login shell may take before Flint gives up on it.
pub const SHELL_TIMEOUT: Duration = Duration::from_secs(3);

/// Directories appended to the inherited `PATH` when the shell cannot be asked.
pub const FALLBACK_DIRS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin"];

const BEGIN_MARKER: &str = "__FLINT_ENV_BEGIN__";
const END_MARKER: &str = "__FLINT_ENV_END__";

/// Where the resolved `PATH` came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSource {
    /// The `path` value in `settings.toml`.
    Settings,
    /// The user's login shell (`$SHELL -ilc env`).
    LoginShell,
    /// The inherited `PATH` plus [`FALLBACK_DIRS`]. Carries the reason the shell was not used.
    Fallback(String),
}

impl PathSource {
    /// Where the `PATH` came from, for messages such as "not found on PATH (PATH from login
    /// shell)".
    pub fn describe(&self) -> &'static str {
        match self {
            PathSource::Settings => "PATH from settings.toml",
            PathSource::LoginShell => "PATH from login shell",
            PathSource::Fallback(_) => "fallback PATH, login shell not used",
        }
    }
}

/// The source of the `PATH` [`apply_process_path`] installed.
static INSTALLED: std::sync::OnceLock<PathSource> = std::sync::OnceLock::new();

/// Where this process's `PATH` came from: [`PathSource::describe`] of what
/// [`apply_process_path`] installed, or "inherited PATH" if it was not called.
pub fn process_path_origin() -> &'static str {
    INSTALLED
        .get()
        .map_or("inherited PATH", PathSource::describe)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPath {
    pub value: String,
    pub source: PathSource,
}

/// Extracts `PATH` from the output of `env`, as printed by an interactive login shell.
///
/// Interactive shells may print banners or prompts around the command's output, so when the
/// begin/end markers are present only the text between them is considered. The last `PATH=`
/// line wins, the same as the shell's own view. Returns `None` if no non-empty `PATH` is found.
pub fn parse_env_path(output: &str) -> Option<String> {
    let section = match output.find(BEGIN_MARKER) {
        Some(start) => {
            let rest = &output[start + BEGIN_MARKER.len()..];
            match rest.find(END_MARKER) {
                Some(end) => &rest[..end],
                None => rest,
            }
        }
        None => output,
    };
    section
        .lines()
        .rev()
        .find_map(|line| line.trim_end_matches('\r').strip_prefix("PATH="))
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
}

/// Returns `current` with each of [`FALLBACK_DIRS`] appended if it is not already present.
pub fn fallback_path(current: &str) -> String {
    let mut parts: Vec<&str> = current.split(':').filter(|p| !p.is_empty()).collect();
    for dir in FALLBACK_DIRS {
        if !parts.contains(dir) {
            parts.push(dir);
        }
    }
    parts.join(":")
}

/// Decides the process `PATH`: the settings override, else the login shell, else the fallback.
///
/// This only spawns the shell and returns the result; it does not change the environment.
pub fn resolve_path(settings_override: Option<&str>) -> ResolvedPath {
    if let Some(path) = settings_override.map(str::trim).filter(|p| !p.is_empty()) {
        return ResolvedPath {
            value: path.to_owned(),
            source: PathSource::Settings,
        };
    }
    match login_shell_path(SHELL_TIMEOUT) {
        Ok(value) => ResolvedPath {
            value,
            source: PathSource::LoginShell,
        },
        Err(reason) => {
            let current = std::env::var("PATH").unwrap_or_default();
            ResolvedPath {
                value: fallback_path(&current),
                source: PathSource::Fallback(reason),
            }
        }
    }
}

/// Resolves the `PATH` and installs it as this process's `PATH`.
///
/// # Safety
///
/// Must be called while the process is single-threaded: before the tokio runtime, GPUI, or any
/// other thread is started (the only exception is described below). `std::env::set_var` is unsound when another thread may read the
/// environment concurrently (for example through libc `getenv`). The only thread this function
/// may leave behind is the reader of a timed-out shell's stdout, which is blocked in `read(2)`
/// and never touches the environment.
pub unsafe fn apply_process_path(settings_override: Option<&str>) -> ResolvedPath {
    let resolved = resolve_path(settings_override);
    // SAFETY: the caller guarantees no other thread reads the environment (see above).
    unsafe { std::env::set_var("PATH", &resolved.value) };
    let _ = INSTALLED.set(resolved.source.clone());
    resolved
}

fn login_shell_path(timeout: Duration) -> Result<String, String> {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "SHELL is not set".to_owned())?;
    let script = format!("echo {BEGIN_MARKER}; env; echo {END_MARKER}");
    let mut command = Command::new(&shell);
    command
        .args(["-ilc", &script])
        // Lets users skip slow parts of their rc files, like VS Code's VSCODE_RESOLVING_ENVIRONMENT.
        .env("FLINT_RESOLVING_ENVIRONMENT", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // SAFETY: `setsid` is async-signal-safe and touches no memory of this process. A new session
    // has no controlling terminal, so an interactive shell cannot take over the terminal Flint was
    // started from (job control, foreground process group), and it leads its own process group,
    // which lets us kill it together with anything its rc files started.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not start {shell}: {e}"))?;

    // Read stdout on a helper thread and stop at the end marker. Waiting for EOF is not enough:
    // daemons started from rc files (ssh-agent, gpg-agent) can inherit stdout and keep it open.
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("flint-path-probe".into())
        .spawn(move || {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        if contains(&buf, END_MARKER.as_bytes()) {
                            break;
                        }
                    }
                }
            }
            let _ = tx.send(buf);
        })
        .map_err(|e| format!("could not start reader thread: {e}"))?;

    let result = rx.recv_timeout(timeout);
    // The shell has printed everything we need (or ran out of time); do not leave it, or
    // anything it started in its process group, running.
    if let Ok(pgid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: plain syscall; the child leads its own process group (setsid above) and has not
        // been reaped yet, so the id cannot have been reused.
        unsafe { libc::killpg(pgid, libc::SIGKILL) };
    }
    let _ = child.kill();
    let _ = child.wait();
    let bytes = result.map_err(|_| format!("{shell} did not answer within {timeout:?}"))?;
    let text = String::from_utf8_lossy(&bytes);
    parse_env_path(&text).ok_or_else(|| format!("{shell} printed no PATH"))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    // covers UP-04, UP-05
    #[test]
    fn path_sources_describe_themselves() {
        assert_eq!(PathSource::LoginShell.describe(), "PATH from login shell");
        assert_eq!(PathSource::Settings.describe(), "PATH from settings.toml");
        assert_eq!(
            PathSource::Fallback("timeout".into()).describe(),
            "fallback PATH, login shell not used"
        );
        // Tests never install a PATH.
        assert_eq!(process_path_origin(), "inherited PATH");
    }

    // covers UP-05
    #[test]
    fn parses_plain_env_output() {
        let out = "HOME=/Users/me\nPATH=/opt/homebrew/bin:/usr/bin:/bin\nSHELL=/bin/zsh\n";
        assert_eq!(
            parse_env_path(out).as_deref(),
            Some("/opt/homebrew/bin:/usr/bin:/bin")
        );
    }

    // covers UP-05
    #[test]
    fn ignores_noise_outside_markers() {
        let out = format!(
            "Welcome!\nPATH=/wrong\n{BEGIN_MARKER}\nUSER=me\nPATH=/right:/bin\n{END_MARKER}\nPATH=/also-wrong\n"
        );
        assert_eq!(parse_env_path(&out).as_deref(), Some("/right:/bin"));
    }

    // covers UP-05
    #[test]
    fn does_not_match_variables_ending_in_path() {
        let out = "MANPATH=/usr/share/man\nINFOPATH=/x\n";
        assert_eq!(parse_env_path(out), None);
    }

    // covers UP-05
    #[test]
    fn handles_crlf_and_multiline_values() {
        let out = "FOO=line one\nPATH=not-a-real-var-line continued\r\nPATH=/a:/b\r\n";
        assert_eq!(parse_env_path(out).as_deref(), Some("/a:/b"));
    }

    // covers UP-05
    #[test]
    fn empty_path_is_none() {
        assert_eq!(parse_env_path("PATH=\n"), None);
        assert_eq!(parse_env_path(""), None);
    }

    // covers UP-05
    #[test]
    fn missing_end_marker_reads_to_end() {
        let out = format!("{BEGIN_MARKER}\nPATH=/x\n");
        assert_eq!(parse_env_path(&out).as_deref(), Some("/x"));
    }

    // covers UP-05
    #[test]
    fn fallback_appends_missing_dirs_once() {
        assert_eq!(
            fallback_path("/usr/bin:/bin"),
            "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin"
        );
        assert_eq!(
            fallback_path("/usr/local/bin:/usr/bin"),
            "/usr/local/bin:/usr/bin:/opt/homebrew/bin"
        );
        assert_eq!(fallback_path(""), "/opt/homebrew/bin:/usr/local/bin");
    }

    // covers UP-05
    #[test]
    fn settings_override_wins() {
        let resolved = resolve_path(Some(" /custom/bin:/usr/bin "));
        assert_eq!(resolved.value, "/custom/bin:/usr/bin");
        assert_eq!(resolved.source, PathSource::Settings);
    }
}
