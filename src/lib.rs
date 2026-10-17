//! Iris HTTP — library surface.
//!
//! P0 keeps the testable, side-effect-free pieces (CLI definition, pidfile
//! handling, signal delivery plumbing) here so they can be unit tested without
//! a running server. The binary entry point (`src/main.rs`) owns the Pingora
//! assembly and process lifecycle.

pub mod config;
pub mod errors;
pub mod firewall;
pub mod lb;
pub mod logging;
pub mod openstack;
pub mod proxy;
pub mod routing;
pub mod state;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};

use crate::errors::AppError;

/// Default configuration root directory (`--config-dir`).
///
/// Per `config.md` §1 this is the *root*: site files live in its `sites/`
/// subdirectory (e.g. `./config/sites`), not directly in `--config-dir`.
pub const DEFAULT_CONFIG_DIR: &str = "./config";

/// Default pidfile for `iris reload` (see `reload.md` §1).
pub const DEFAULT_PIDFILE: &str = "/run/iris/iris.pid";

/// Top-level CLI definition: `iris <command>`.
#[derive(Debug, Parser)]
#[command(name = "iris", version, about = "Iris HTTP reverse proxy", long_about = None)]
pub struct Cli {
    /// Subcommand to execute.
    #[command(subcommand)]
    pub command: Command,
}

/// Supported subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the proxy server.
    Run(RunArgs),
    /// Reload the running server's configuration (sends `SIGHUP`).
    Reload(ReloadArgs),
}

/// Arguments for `iris run`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Configuration root directory; site files are read from its `sites/`
    /// subdirectory (`config.md` §1).
    #[arg(long, value_name = "DIR", default_value = DEFAULT_CONFIG_DIR)]
    pub config_dir: PathBuf,

    /// Write the process id to this file at startup.
    #[arg(long, value_name = "PATH")]
    pub pidfile: Option<PathBuf>,

    /// Watch the configuration directory and reload automatically on change.
    #[arg(long)]
    pub watch_config: bool,
}

/// Arguments for `iris reload`.
#[derive(Debug, Args)]
pub struct ReloadArgs {
    /// Pidfile of the running instance to signal.
    #[arg(long, value_name = "PATH", default_value = DEFAULT_PIDFILE)]
    pub pidfile: PathBuf,
}

/// Parse a [`Cli`] from an explicit argument iterator.
///
/// A thin wrapper over [`Cli::try_parse_from`] so argument parsing is unit
/// testable without touching the real process arguments.
pub fn parse_cli_from<I, T>(args: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    Cli::try_parse_from(args)
}

/// Read and validate the PID stored in `pidfile`.
///
/// Returns [`AppError::PidFileIo`] when the file cannot be read (missing file,
/// directory instead of file, permission error) and [`AppError::InvalidPid`]
/// when it does not contain a positive integer.
pub fn read_pid(pidfile: &Path) -> Result<i32, AppError> {
    let raw = std::fs::read_to_string(pidfile).map_err(|e| AppError::pid_file_io(pidfile, e))?;
    let trimmed = raw.trim();
    let pid: i32 = trimmed
        .parse()
        .map_err(|_| AppError::invalid_pid(pidfile, trimmed))?;
    if pid <= 0 {
        return Err(AppError::invalid_pid(pidfile, trimmed));
    }
    Ok(pid)
}

/// Send `SIGHUP` to `pid`.
///
/// Uses the POSIX `kill(1)` utility so no additional signal-sending crate is
/// needed (the P0 dependency set is fixed by `plan/03`+`plan/13`).
pub fn send_sighup(pid: i32) -> Result<(), AppError> {
    let status = std::process::Command::new("kill")
        .arg("-HUP")
        .arg(pid.to_string())
        .status()
        .map_err(|e| AppError::signal_spawn(pid, e))?;
    if status.success() {
        Ok(())
    } else {
        Err(AppError::signal_failed(pid, status.to_string()))
    }
}

/// Reload logic with an injectable signal sender.
///
/// Reads the pidfile then invokes `send`. Tests pass a fake sender to verify the
/// signal path without delivering a real `SIGHUP`.
pub fn reload_with<F>(pidfile: &Path, send: F) -> Result<i32, AppError>
where
    F: FnOnce(i32) -> Result<(), AppError>,
{
    let pid = read_pid(pidfile)?;
    send(pid)?;
    Ok(pid)
}

/// Reload the running instance identified by `pidfile` by sending `SIGHUP`.
pub fn reload(pidfile: &Path) -> Result<i32, AppError> {
    reload_with(pidfile, send_sighup)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    // ----- CLI parsing (plan/03 TDD list: default & explicit --config-dir) -----

    #[test]
    fn run_defaults_config_dir_and_flags() {
        let cli = parse_cli_from(["iris", "run"]).expect("run parses");
        match cli.command {
            Command::Run(args) => {
                assert_eq!(args.config_dir, PathBuf::from("./config"));
                assert_eq!(args.pidfile, None);
                assert!(!args.watch_config);
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn run_parses_explicit_config_dir_pidfile_and_watch() {
        let cli = parse_cli_from([
            "iris",
            "run",
            "--config-dir",
            "/etc/iris",
            "--pidfile",
            "/run/iris.pid",
            "--watch-config",
        ])
        .expect("run parses");
        match cli.command {
            Command::Run(args) => {
                assert_eq!(args.config_dir, PathBuf::from("/etc/iris"));
                assert_eq!(args.pidfile, Some(PathBuf::from("/run/iris.pid")));
                assert!(args.watch_config);
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn reload_default_pidfile() {
        let cli = parse_cli_from(["iris", "reload"]).expect("reload parses");
        match cli.command {
            Command::Reload(args) => {
                assert_eq!(args.pidfile, PathBuf::from("/run/iris/iris.pid"));
            }
            other => panic!("expected Reload, got {other:?}"),
        }
    }

    #[test]
    fn reload_explicit_pidfile() {
        let cli = parse_cli_from(["iris", "reload", "--pidfile", "/tmp/iris.pid"])
            .expect("reload parses");
        match cli.command {
            Command::Reload(args) => {
                assert_eq!(args.pidfile, PathBuf::from("/tmp/iris.pid"));
            }
            other => panic!("expected Reload, got {other:?}"),
        }
    }

    #[test]
    fn missing_subcommand_is_an_error() {
        assert!(parse_cli_from(["iris"]).is_err());
    }

    #[test]
    fn unknown_subcommand_is_an_error() {
        assert!(parse_cli_from(["iris", "frobnicate"]).is_err());
    }

    // ----- pidfile handling -----

    #[test]
    fn read_pid_missing_file_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope.pid");
        let err = read_pid(&missing).expect_err("missing pidfile must error");
        assert!(matches!(err, AppError::PidFileIo { .. }));
    }

    #[test]
    fn read_pid_directory_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = read_pid(dir.path()).expect_err("directory is not a pidfile");
        assert!(matches!(err, AppError::PidFileIo { .. }));
    }

    #[test]
    fn read_pid_corrupt_content_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bad.pid");
        std::fs::write(&path, "not-a-pid\n").expect("write");
        let err = read_pid(&path).expect_err("corrupt pidfile must error");
        match err {
            AppError::InvalidPid { content, .. } => assert_eq!(content, "not-a-pid"),
            other => panic!("expected InvalidPid, got {other:?}"),
        }
    }

    #[test]
    fn read_pid_empty_and_non_positive_error() {
        let dir = tempfile::tempdir().expect("tempdir");

        let empty = dir.path().join("empty.pid");
        std::fs::write(&empty, "   \n").expect("write");
        assert!(matches!(
            read_pid(&empty),
            Err(AppError::InvalidPid { .. })
        ));

        let zero = dir.path().join("zero.pid");
        std::fs::write(&zero, "0\n").expect("write");
        assert!(matches!(read_pid(&zero), Err(AppError::InvalidPid { .. })));

        let negative = dir.path().join("neg.pid");
        std::fs::write(&negative, "-7\n").expect("write");
        assert!(matches!(
            read_pid(&negative),
            Err(AppError::InvalidPid { .. })
        ));
    }

    #[test]
    fn read_pid_valid_trims_whitespace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ok.pid");
        std::fs::write(&path, " 4321\n").expect("write");
        assert_eq!(read_pid(&path).expect("valid pid"), 4321);
    }

    // ----- reload signal path (no real signal delivered) -----

    #[test]
    fn reload_with_valid_pidfile_invokes_sender() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ok.pid");
        std::fs::write(&path, "1234\n").expect("write");

        let seen = Cell::new(None);
        let pid = reload_with(&path, |p| {
            seen.set(Some(p));
            Ok(())
        })
        .expect("reload succeeds");

        assert_eq!(pid, 1234);
        assert_eq!(seen.get(), Some(1234));
    }

    #[test]
    fn reload_with_missing_pidfile_does_not_send() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("missing.pid");

        let called = Cell::new(false);
        let err = reload_with(&missing, |_| {
            called.set(true);
            Ok(())
        })
        .expect_err("must fail before sending");

        assert!(matches!(err, AppError::PidFileIo { .. }));
        assert!(!called.get(), "sender must not run when pidfile is invalid");
    }

    #[test]
    fn reload_with_propagates_sender_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ok.pid");
        std::fs::write(&path, "99\n").expect("write");

        let err = reload_with(&path, |p| Err(AppError::signal_failed(p, "boom")))
            .expect_err("sender failure propagates");
        assert!(matches!(err, AppError::SignalFailed { pid: 99, .. }));
    }
}
