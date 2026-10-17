//! Application-level error base for Iris HTTP.
//!
//! P0 only needs the control-surface errors (CLI + reload). Later stages add
//! domain errors (`ConfigError`, `ResolveError`, ...) per `02-contracts.md`;
//! this module is the shared foundation those can build on. It implements
//! [`std::error::Error`] so it composes with `anyhow` at the process boundary.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Errors raised by the Iris HTTP control surface (CLI, reload, bootstrap).
#[derive(Debug)]
pub enum AppError {
    /// Could not read the pidfile contents.
    PidFileIo {
        /// The pidfile that could not be read.
        path: PathBuf,
        /// The underlying I/O failure.
        source: io::Error,
    },
    /// The pidfile was readable but did not contain a valid positive PID.
    InvalidPid {
        /// The pidfile whose contents were invalid.
        path: PathBuf,
        /// The (trimmed) offending contents.
        content: String,
    },
    /// Failed to spawn the process that delivers the signal.
    SignalSpawn {
        /// Target process id.
        pid: i32,
        /// The underlying I/O failure.
        source: io::Error,
    },
    /// The signal delivery command exited with a non-zero status.
    SignalFailed {
        /// Target process id.
        pid: i32,
        /// The human-readable exit status of the signalling command.
        status: String,
    },
}

impl AppError {
    /// Construct a [`AppError::PidFileIo`].
    pub fn pid_file_io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::PidFileIo {
            path: path.into(),
            source,
        }
    }

    /// Construct a [`AppError::InvalidPid`].
    pub fn invalid_pid(path: impl Into<PathBuf>, content: impl Into<String>) -> Self {
        Self::InvalidPid {
            path: path.into(),
            content: content.into(),
        }
    }

    /// Construct a [`AppError::SignalSpawn`].
    pub fn signal_spawn(pid: i32, source: io::Error) -> Self {
        Self::SignalSpawn { pid, source }
    }

    /// Construct a [`AppError::SignalFailed`].
    pub fn signal_failed(pid: i32, status: impl Into<String>) -> Self {
        Self::SignalFailed {
            pid,
            status: status.into(),
        }
    }

    /// The pidfile path associated with this error, when applicable.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::PidFileIo { path, .. } | Self::InvalidPid { path, .. } => Some(path.as_path()),
            Self::SignalSpawn { .. } | Self::SignalFailed { .. } => None,
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PidFileIo { path, source } => {
                write!(f, "failed to read pidfile '{}': {source}", path.display())
            }
            Self::InvalidPid { path, content } => write!(
                f,
                "pidfile '{}' does not contain a valid PID: {content:?}",
                path.display()
            ),
            Self::SignalSpawn { pid, source } => {
                write!(f, "failed to send SIGHUP to pid {pid}: {source}")
            }
            Self::SignalFailed { pid, status } => {
                write!(f, "failed to send SIGHUP to pid {pid}: command exited with {status}")
            }
        }
    }
}

impl std::error::Error for AppError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::PidFileIo { source, .. } | Self::SignalSpawn { source, .. } => Some(source),
            Self::InvalidPid { .. } | Self::SignalFailed { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_pid_display_includes_path() {
        let err = AppError::invalid_pid("/run/iris/iris.pid", "not-a-pid");
        let msg = err.to_string();
        assert!(msg.contains("/run/iris/iris.pid"), "msg was: {msg}");
        assert!(msg.contains("not-a-pid"), "msg was: {msg}");
        assert_eq!(err.path(), Some(Path::new("/run/iris/iris.pid")));
    }

    #[test]
    fn signal_failed_is_not_a_path_error() {
        let err = AppError::signal_failed(42, "exit status: 1");
        assert!(err.path().is_none());
        assert!(err.to_string().contains("42"));
    }
}
