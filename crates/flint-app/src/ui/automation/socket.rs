//! The automation socket: a Unix socket at `FLINT_CONFIG_DIR/automation.sock` (see
//! [`super::setup`]), mode 0600, that accepts only the user running Flint. Each connection is
//! served on its own thread, one request line at a time; the line goes to `handle`, which answers
//! with one response line.

use std::ffi::CString;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The longest request line accepted (1 MiB), so a stray client cannot exhaust memory.
const MAX_LINE: u64 = 1 << 20;

/// The socket file this process created, so quitting removes it only if it is still ours.
#[derive(Debug, Clone)]
pub struct SocketFile {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl SocketFile {
    /// Removes the socket, unless something else has replaced it since.
    pub fn remove(&self) {
        if let Ok(meta) = fs::symlink_metadata(&self.path)
            && meta.dev() == self.dev
            && meta.ino() == self.ino
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Binds the socket at `path`, readable and writable only by its owner.
///
/// - The socket is bound in a fresh private directory (`mkdtemp`, mode 0700) and restricted
///   there, so nobody can connect before its mode is set.
/// - It is moved into place with `link` and `unlink`, which never replaces an existing file.
/// - A socket left by an earlier run is removed, unless something still answers on it: then
///   another Flint is serving there and this one refuses to start. Any other file is left alone.
pub fn bind(path: &Path) -> io::Result<(UnixListener, SocketFile)> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "socket path has no directory")
    })?;
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            if UnixStream::connect(path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("{} is live: another Flint is serving there", path.display()),
                ));
            }
            fs::remove_file(path)?;
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
    let staging = make_temp_dir(dir)?;
    let staged = staging.join("s");
    let result = (|| {
        let listener = UnixListener::bind(&staged)?;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o600))?;
        fs::hard_link(&staged, path)?;
        let meta = fs::symlink_metadata(path)?;
        Ok((
            listener,
            SocketFile {
                path: path.to_owned(),
                dev: meta.dev(),
                ino: meta.ino(),
            },
        ))
    })();
    let _ = fs::remove_file(&staged);
    let _ = fs::remove_dir(&staging);
    result
}

/// A new private directory in `dir`, with a short name (socket paths are limited in length).
fn make_temp_dir(dir: &Path) -> io::Result<PathBuf> {
    let template = CString::new(dir.join(".a-XXXXXX").as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut bytes = template.into_bytes_with_nul();
    // SAFETY: `bytes` is a NUL-terminated template that mkdtemp fills in place.
    let made = unsafe { libc::mkdtemp(bytes.as_mut_ptr().cast()) };
    if made.is_null() {
        return Err(io::Error::last_os_error());
    }
    bytes.pop();
    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
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
    fn binds_owner_only_and_answers_line_by_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("automation.sock");
        // A stale socket from an earlier run (nobody listening) is replaced.
        drop(UnixListener::bind(&path).unwrap());
        let (listener, _file) = bind(&path).unwrap();
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
    fn refuses_a_live_socket_and_other_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("automation.sock");
        let _live = UnixListener::bind(&path).unwrap();
        let error = bind(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(path.exists());

        let other = dir.path().join("other.sock");
        fs::write(&other, "not a socket").unwrap();
        assert!(bind(&other).is_err());
        assert_eq!(fs::read_to_string(&other).unwrap(), "not a socket");
    }

    #[test]
    fn removes_only_its_own_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("automation.sock");
        let (listener, file) = bind(&path).unwrap();
        drop(listener);
        file.remove();
        assert!(!path.exists());

        // Replaced by someone else since: left alone.
        let (listener, file) = bind(&path).unwrap();
        drop(listener);
        fs::remove_file(&path).unwrap();
        fs::write(&path, "theirs").unwrap();
        file.remove();
        assert_eq!(fs::read_to_string(&path).unwrap(), "theirs");
    }
}
