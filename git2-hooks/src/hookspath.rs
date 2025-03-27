use git2::Repository;

use crate::{error::Result, HookResult, HooksError};

use std::{
	path::{Path, PathBuf},
	process::Command,
	str::FromStr,
};

pub struct HookPaths {
	pub git: PathBuf,
	pub hook: PathBuf,
	pub pwd: PathBuf,
}

const CONFIG_HOOKS_PATH: &str = "core.hooksPath";
const DEFAULT_HOOKS_PATH: &str = "hooks";
const ENOEXEC: i32 = 8;

impl HookPaths {
	/// `core.hooksPath` always takes precedence.
	/// If its defined and there is no hook `hook` this is not considered
	/// an error or a reason to search in other paths.
	/// If the config is not set we go into search mode and
	/// first check standard `.git/hooks` folder and any sub path provided in `other_paths`.
	///
	/// Note: we try to model as closely as possible what git shell is doing.
	pub fn new(
		repo: &Repository,
		other_paths: Option<&[&str]>,
		hook: &str,
	) -> Result<Self> {
		let pwd = repo
			.workdir()
			.unwrap_or_else(|| repo.path())
			.to_path_buf();

		let git_dir = repo.path().to_path_buf();

		if let Some(config_path) = Self::config_hook_path(repo)? {
			let hooks_path = PathBuf::from(config_path);

			let hook = hooks_path.join(hook);

			let hook = shellexpand::full(
				hook.as_os_str()
					.to_str()
					.ok_or(HooksError::PathToString)?,
			)?;

			let hook = PathBuf::from_str(hook.as_ref())
				.map_err(|_| HooksError::PathToString)?;

			return Ok(Self {
				git: git_dir,
				hook,
				pwd,
			});
		}

		Ok(Self {
			git: git_dir,
			hook: Self::find_hook(repo, other_paths, hook),
			pwd,
		})
	}

	fn config_hook_path(repo: &Repository) -> Result<Option<String>> {
		Ok(repo.config()?.get_string(CONFIG_HOOKS_PATH).ok())
	}

	/// check default hook path first and then followed by `other_paths`.
	/// if no hook is found we return the default hook path
	fn find_hook(
		repo: &Repository,
		other_paths: Option<&[&str]>,
		hook: &str,
	) -> PathBuf {
		let mut paths = vec![DEFAULT_HOOKS_PATH.to_string()];
		if let Some(others) = other_paths {
			paths.extend(
				others
					.iter()
					.map(|p| p.trim_end_matches('/').to_string()),
			);
		}

		for p in paths {
			let p = repo.path().to_path_buf().join(p).join(hook);
			if p.exists() {
				return p;
			}
		}

		repo.path()
			.to_path_buf()
			.join(DEFAULT_HOOKS_PATH)
			.join(hook)
	}

	/// was a hook file found and is it executable
	pub fn found(&self) -> bool {
		self.hook.exists() && is_executable(&self.hook)
	}

	/// this function calls hook scripts based on conventions documented here
	/// see <https://git-scm.com/docs/githooks>
	pub fn run_hook(&self, args: &[&str]) -> Result<HookResult> {
		let hook = self.hook.clone();
		log::trace!("run hook '{:?}' in '{:?}'", hook, self.pwd);

		let run_command = |command: &mut Command| {
			command
				.args(args)
				.current_dir(&self.pwd)
				.with_no_window()
				.output()
		};

		let output = if cfg!(windows) {
			// execute hook in shell
			let command = {
				let mut os_str = std::ffi::OsString::new();
				os_str.push("'");
				if let Some(hook) = hook.to_str() {
					// SEE: https://pubs.opengroup.org/onlinepubs/9699919799/utilities/V3_chap02.html#tag_18_02_02
					// Enclosing characters in single-quotes ( '' ) shall preserve the literal value of each character within the single-quotes.
					// A single-quote cannot occur within single-quotes.
					const REPLACEMENT: &str = concat!(
						"'",   // closing single-quote
						"\\'", // one escaped single-quote (outside of single-quotes)
						"'",   // new single-quote
					);
					os_str.push(hook.replace('\'', REPLACEMENT));
				} else {
					os_str.push(hook.as_os_str()); // TODO: this doesn't work if `hook` contains single-quotes
				}
				os_str.push("'");
				os_str.push(" \"$@\"");

				os_str
			};
			run_command(
				sh_command().arg("-c").arg(command).arg(&hook),
			)
		} else {
			// execute hook directly
			match run_command(&mut Command::new(&hook)) {
				Err(err) if err.raw_os_error() == Some(ENOEXEC) => {
					run_command(sh_command().arg(&hook))
				}
				result => result,
			}
		}?;

		if output.status.success() {
			Ok(HookResult::Ok { hook })
		} else {
			let stderr =
				String::from_utf8_lossy(&output.stderr).to_string();
			let stdout =
				String::from_utf8_lossy(&output.stdout).to_string();

			Ok(HookResult::RunNotSuccessful {
				code: output.status.code(),
				stdout,
				stderr,
				hook,
			})
		}
	}
}

fn sh_command() -> Command {
	let mut command = Command::new(sh_path());

	if cfg!(windows) {
		// This call forces Command to handle the Path environment correctly on windows,
		// the specific env set here does not matter
		// see https://github.com/rust-lang/rust/issues/37519
		command.env(
			"DUMMY_ENV_TO_FIX_WINDOWS_CMD_RUNS",
			"FixPathHandlingOnWindows",
		);

		// Use -l to avoid "command not found"
		command.arg("-l");
	}

	command
}

/// Get the path to the sh executable.
/// On Windows get the sh.exe bundled with Git for Windows
pub fn sh_path() -> PathBuf {
	if cfg!(windows) {
		Command::new("where.exe")
			.arg("git")
			.output()
			.ok()
			.map(|out| {
				PathBuf::from(Into::<String>::into(
					String::from_utf8_lossy(&out.stdout),
				))
			})
			.as_deref()
			.and_then(Path::parent)
			.and_then(Path::parent)
			.map(|p| p.join("usr/bin/sh.exe"))
			.filter(|p| p.exists())
			.unwrap_or_else(|| "sh".into())
	} else {
		"sh".into()
	}
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
	use std::os::unix::fs::PermissionsExt;

	let metadata = match path.metadata() {
		Ok(metadata) => metadata,
		Err(e) => {
			log::error!("metadata error: {}", e);
			return false;
		}
	};

	let permissions = metadata.permissions();

	permissions.mode() & 0o111 != 0
}

#[cfg(windows)]
/// windows does not consider shell scripts to be executable so we consider everything
/// to be executable (which is not far from the truth for windows platform.)
const fn is_executable(_: &Path) -> bool {
	true
}

trait CommandExt {
	/// The process is a console application that is being run without a
	/// console window. Therefore, the console handle for the application is
	/// not set.
	///
	/// This flag is ignored if the application is not a console application,
	/// or if it used with either `CREATE_NEW_CONSOLE` or `DETACHED_PROCESS`.
	///
	/// See: <https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags>
	const CREATE_NO_WINDOW: u32 = 0x0800_0000;

	fn with_no_window(&mut self) -> &mut Self;
}

impl CommandExt for Command {
	/// On Windows, CLI applications that aren't the window's subsystem will
	/// create and show a console window that pops up next to the main
	/// application window when run. We disable this behavior by setting the
	/// `CREATE_NO_WINDOW` flag.
	#[inline]
	fn with_no_window(&mut self) -> &mut Self {
		#[cfg(windows)]
		{
			use std::os::windows::process::CommandExt;
			self.creation_flags(Self::CREATE_NO_WINDOW);
		}

		self
	}
}
