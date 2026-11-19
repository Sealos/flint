//! The automation socket: a Unix socket at `FLINT_CONFIG_DIR/automation.sock`, mode 0600,
//! that accepts only the user running Flint. Each connection is served on its own thread, one
//! request line at a time; the line goes to `handle`, which answers with one response line.

use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The socket's file name in the config directory.
pub const SOCKET_NAME: &str = "automation.sock";

/// The longest request line accepted (1 MiB), so a stray client cannot exhaust memory.
const MAX_LINE: u64 = 1 << 20;

/// Where the socket goes: `FLINT_CONFIG_DIR/automation.sock`. Without `FLINT_CONFIG_DIR` there
/// is no socket, so the channel never sits in the user's real config directory.
pub fn socket_path(config_dir: Option<OsString>) -> Result<PathBuf, String> {
    match config_dir {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir).join(SOCKET_NAME)),
        _ => Err(
            "FLINT_CONFIG_DIR is not set; the automation channel needs a scratch config \
                  directory and will not use the real one"
                .to_owned(),
        ),
    }
}

/// Binds the socket at `path`, readable and writable only by its owner. The socket is made in
/// a private directory, restricted, then moved into place, so nobody can connect before its
/// mode is set. A stale socket from an earlier run is replaced; any other file is left alone.
pub fn bind(path: &Path) -> io::Result<UnixListener> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "socket path has no directory")
    })?;
    fs::create_dir_all(dir)?;
    match fs::symlink_metadata(path) {
        Ok(meta) if std::os::unix::fs::FileTypeExt::is_socket(&meta.file_type()) => {
            fs::remove_file(path)?
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let staging = dir.join(format!(".automation-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::DirBuilder::new().mode(0o700).create(&staging)?;
    let staged = staging.join(SOCKET_NAME);
    let result = (|| {
        let listener = UnixListener::bind(&staged)?;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o600))?;
        fs::rename(&staged, path)?;
        Ok(listener)
    })();
    let _ = fs::remove_dir_all(&staging);
    result
}

/// Accepts connections on a background thread, each served on its own thread.
pub fn serve(listener: UnixListener, handle: impl Fn(&str) -> String + Send + Sync + 'static) {
    let handle = Arc::new(handle);
    let spawned = std::thread::Builder::new()
        .name("automation-accept".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let stream = match stream {
                    Ok(stream) => stream,
                    Err(e) => {
                        tracing::warn!("automation: accept failed: {e}");
                        continue;
                    }
                };
                if !same_user(&stream) {
                    tracing::warn!("automation: refused a connection from another user");
                    continue;
                }
                let handle = handle.clone();
                let _ = std::thread::Builder::new()
                    .name("automation-conn".into())
                    .spawn(move || {
                        if let Err(e) = serve_connection(stream, &*handle) {
                            tracing::debug!("automation: connection ended: {e}");
                        }
                    });
            }
        });
    if let Err(e) = spawned {
        tracing::error!("automation: could not start the accept thread: {e}");
    }
}

/// Reads request lines and writes one response line for each, until the client hangs up.
fn serve_connection(stream: UnixStream, handle: &dyn Fn(&str) -> String) -> io::Result<()> {
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        let read = (&mut reader).take(MAX_LINE).read_line(&mut line)?;
        if read == 0 {
            return Ok(());
        }
        if !line.ends_with('\n') && read as u64 == MAX_LINE {
            writer.write_all(b"{\"id\":null,\"ok\":false,\"error\":{\"code\":\"bad_request\",\"message\":\"request line too long\"}}\n")?;
            return Ok(());
        }
        let request = line.trim();
        if request.is_empty() {
            continue;
        }
        let mut response = handle(request);
        response.push('\n');
        writer.write_all(response.as_bytes())?;
        writer.flush()?;
    }
}

/// Whether the peer runs as this process's user. The 0600 mode already says so; this guards
/// against a socket file whose mode was changed after the fact.
fn same_user(stream: &UnixStream) -> bool {
    let mut uid = 0;
    let mut gid = 0;
    // SAFETY: getpeereid only writes the two out-parameters, for a socket fd we own.
    let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    // SAFETY: geteuid has no preconditions.
    rc == 0 && uid == unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;

    use super::*;

    #[test]
    fn refuses_without_a_config_dir() {
        assert!(socket_path(None).is_err());
        assert!(socket_path(Some(OsString::new())).is_err());
        assert_eq!(
            socket_path(Some("/tmp/x".into())).unwrap(),
            PathBuf::from("/tmp/x/automation.sock")
        );
    }

    #[test]
    fn binds_owner_only_and_answers_line_by_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SOCKET_NAME);
        // A stale socket from an earlier run is replaced.
        drop(UnixListener::bind(&path).unwrap());
        let listener = bind(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            1,
            "staging dir left behind"
        );

        serve(listener, |line| format!("echo {line}"));
        let mut stream = UnixStream::connect(&path).unwrap();
        stream.write_all(b"one\n\ntwo\n").unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut answer = String::new();
        reader.read_line(&mut answer).unwrap();
        assert_eq!(answer, "echo one\n");
        answer.clear();
        reader.read_line(&mut answer).unwrap();
        assert_eq!(answer, "echo two\n");
    }

    #[test]
    fn leaves_other_files_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SOCKET_NAME);
        fs::write(&path, "not a socket").unwrap();
        assert!(bind(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "not a socket");
    }
}
