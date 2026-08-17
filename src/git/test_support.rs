//! Test-only helpers to run `git` without picking up the machine's git configuration.
//!
//! The tests build throwaway repositories, so whatever config the machine carries changes
//! what those commands do: `tag.gpgSign` alone turns the lightweight `git tag v1.0` into an
//! annotated tag, which then fails with `fatal: no tag message?`.
//!
//! This is not only a developer-machine problem, though that is where it bites first. Every
//! CI runner carries config too — `core.autocrlf=true` and `init.defaultBranch=master` in
//! the system config on Windows, `safe.directory`, an LFS filter and a wall of `advice.*` on
//! macOS — which today happens to miss what these fixtures depend on. The isolation is what
//! keeps that a coincidence rather than a dependency.
//!
//! Every git invocation in *this crate's* unit tests should go through [`run_git`] (for
//! the commands that set up a fixture) or [`IsolatedGitExecutor`] (for the commands rv
//! itself runs). The integration tests in `tests/` cannot: this module is `#[cfg(test)]`,
//! so it does not exist for them, and they drive the rv binary, which uses the production
//! [`GitExecutor`] and therefore the machine's config. `tests/cli_global_cache.rs` syncs a
//! real git dependency that way.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use crate::git::{CommandExecutor, GitExecutor};

const TEST_USER_NAME: &str = "Test User";
const TEST_USER_EMAIL: &str = "test@example.com";

/// An empty config file, in a directory this process owns, kept for the lifetime of the
/// test binary and removed when it exits.
///
/// It has to be a path no one else can write to, not merely a path that happens not to
/// exist: git reads whatever `GIT_CONFIG_GLOBAL` points at, so a well-known name under the
/// shared temp directory (`/tmp` on Linux) is one `touch` by any other user away from
/// feeding the tests an attacker's config, and `core.pager`, `alias.*` and `include.path`
/// all run commands. `tempfile` creates the directory with owner-only permissions.
fn empty_config_file() -> PathBuf {
    static EMPTY_CONFIG_DIR: OnceLock<tempfile::TempDir> = OnceLock::new();

    let dir = EMPTY_CONFIG_DIR.get_or_init(|| {
        let dir = tempfile::tempdir().expect("failed to create the git test isolation dir");
        std::fs::write(dir.path().join("gitconfig"), "")
            .expect("failed to create the empty git config");
        dir
    });

    dir.path().join("gitconfig")
}

/// Makes a git command ignore the machine's configuration and environment.
pub(crate) fn isolate_git_env(command: &mut Command) -> &mut Command {
    let empty_config = empty_config_file();

    // Pinning `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` below settles where git reads config
    // from, whatever `HOME` says, so this is not what hides the user's `.gitconfig`. It is
    // here for the rest of the `GIT_*` namespace, which that pinning does not cover:
    // `GIT_ASKPASS`, `GIT_EDITOR` and `GIT_TERMINAL_PROMPT` are all set in a developer
    // shell, and `GIT_DIR` or `GIT_INDEX_FILE` would point the fixtures at another
    // repository outright. The CI runners set none of them — this one is for us.
    command.env_clear();

    // The one variable worth keeping, and not because the spawn needs it: on Unix a cleared
    // PATH falls back to `confstr(_CS_PATH)`, and on Windows `CreateProcess` resolves the
    // image from the parent's PATH whatever the child's environment says. It is *which* git
    // that finds. On the macOS CI runner the fallback quietly drops from Homebrew's git
    // 2.55.0 to Apple Git 2.50.1 inside Xcode, five minor versions back, and every fixture
    // command still succeeds — so without this line the suite keeps passing against a git
    // nobody chose, and nothing here would say so. PATH cannot affect git's configuration.
    // Left unset rather than set to an empty string when the parent has no PATH, since an
    // empty PATH turns that fallback off.
    //
    // Nothing else survives the clear, Windows included. The usual reason to hand some of it
    // back there is that Git for Windows is an msys2 binary whose linked Windows APIs want
    // `SystemRoot`, `SystemDrive`, `WINDIR`, `TEMP`, `TMP`, `COMSPEC` and `PATHEXT` — for
    // what these fixtures do, they do not. Withholding each in turn, and all of them at once,
    // leaves both the outcome and the resolved binary of the full sequence (bare `init`,
    // `clone`, `add`, `commit`, `push`, `tag`, `fetch`) unchanged, on the official Git for
    // Windows build and on msys2's alike. None of that touches the network, so a unit test
    // that ever fetches over one may want `SystemRoot` back for winsock.
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }

    command
        // Taking `HOME`/`XDG_CONFIG_HOME` away already hides the global config, but only
        // for as long as nothing puts `HOME` back, so pin it. GIT_CONFIG_GLOBAL replaces
        // both `~/.gitconfig` and `$XDG_CONFIG_HOME/git/config`, so those need no handling
        // of their own.
        .env("GIT_CONFIG_GLOBAL", &empty_config)
        // The system config is found by compiled-in path, so `env_clear` does not stop git
        // from reading it: without this the tests see whatever is in `/etc/gitconfig` or,
        // under Homebrew, `$(brew --prefix)/etc/gitconfig`.
        .env("GIT_CONFIG_SYSTEM", &empty_config)
        // GIT_CONFIG_SYSTEM only exists since git 2.32
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        // Without a user config there is no user.name/user.email to commit or tag with
        .env("GIT_AUTHOR_NAME", TEST_USER_NAME)
        .env("GIT_AUTHOR_EMAIL", TEST_USER_EMAIL)
        .env("GIT_COMMITTER_NAME", TEST_USER_NAME)
        .env("GIT_COMMITTER_EMAIL", TEST_USER_EMAIL)
        // No credential prompts: a test that blocks on one never finishes
        .env("GIT_TERMINAL_PROMPT", "0")
}

/// The [`GitExecutor`] rv uses in production, with the test isolation applied to every
/// command before it runs.
///
/// The isolation lands after the calling code has built its command, so the `env_clear` in
/// [`isolate_git_env`] drops whatever environment that code set up: `fetch_with_cli`'s
/// `GIT_TERMINAL_PROMPT` and `GIT_DIR` handling is replaced by the equivalent here rather
/// than exercised. Anything the production commands grow beyond those needs mirroring in
/// [`isolate_git_env`], or the tests quietly stop running what rv runs.
#[derive(Debug, Clone)]
pub(crate) struct IsolatedGitExecutor;

impl CommandExecutor for IsolatedGitExecutor {
    fn execute(&self, command: &mut Command) -> Result<String, std::io::Error> {
        GitExecutor.execute(isolate_git_env(command))
    }
}

/// Runs a git command in `dir` to set up a test fixture, panicking if it fails.
///
/// Note that this is not a test itself: the assert is there so a failing fixture command
/// points at what went wrong instead of at the test that used its result.
pub(crate) fn run_git(args: &[&str], dir: &Path) {
    let mut command = Command::new("git");
    command.args(args).current_dir(dir);
    let output = isolate_git_env(&mut command).output().unwrap();
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lists the configuration an isolated git command can see, run in an empty directory
    /// so nothing but the isolation decides the answer. `extra_env` is layered on top of
    /// [`isolate_git_env`], which is how the tests below put back the variables the
    /// isolation is supposed to keep winning over.
    fn isolated_config_list(extra_env: &[(&str, &Path)]) -> String {
        let dir = tempfile::tempdir().unwrap();
        let mut command = Command::new("git");
        command
            .arg("config")
            .arg("--list")
            .arg("--show-scope")
            .current_dir(dir.path());

        isolate_git_env(&mut command);
        for (name, value) in extra_env {
            command.env(name, value);
        }

        let output = command.output().unwrap();
        // Without this the test passes on an empty stdout, which is exactly what a git too
        // old for `--show-scope` (< 2.26) produces before failing.
        assert!(
            output.status.success(),
            "git config --list --show-scope failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn assert_nothing_outside_the_repository_leaked(listed: &str, context: &str) {
        let leaked: Vec<_> = listed
            .lines()
            .filter(|line| line.starts_with("global") || line.starts_with("system"))
            .collect();

        assert!(
            leaked.is_empty(),
            "git config from outside the repository is visible to the tests ({context}):\n{}",
            leaked.join("\n")
        );
    }

    /// Guards the isolation itself: without [`isolate_git_env`] the same command lists the
    /// `global` and `system` entries of whatever machine it runs on, and there has never
    /// been a machine here with none. The CI runners listed 24 such entries on macOS, 14 on
    /// Windows and 5 on Linux, so this is not the local-only check it looks like — it is
    /// load-bearing everywhere, and it fails on the *isolation* breaking rather than on the
    /// machine being unusual.
    #[test]
    fn the_machines_config_is_not_visible() {
        assert_nothing_outside_the_repository_leaked(
            &isolated_config_list(&[]),
            "the machine's own config",
        );
    }

    /// The companion that fails everywhere, CI included: it brings its own config instead
    /// of relying on the developer having one. `env_clear` alone would not be enough here,
    /// since these are set again afterwards — this is what pinning `GIT_CONFIG_GLOBAL`
    /// buys, and it has to keep winning over both places git looks for a global config.
    #[test]
    fn a_restored_home_does_not_bring_a_global_config_back() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".gitconfig"),
            "[user]\n\tname = leaked-from-home\n",
        )
        .unwrap();

        let listed = isolated_config_list(&[("HOME", home.path())]);
        assert_nothing_outside_the_repository_leaked(&listed, "HOME put back");
        assert!(
            !listed.contains("leaked-from-home"),
            "$HOME/.gitconfig is visible to the tests:\n{listed}"
        );

        let xdg = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(xdg.path().join("git")).unwrap();
        std::fs::write(
            xdg.path().join("git").join("config"),
            "[user]\n\tname = leaked-from-xdg\n",
        )
        .unwrap();

        let listed = isolated_config_list(&[("XDG_CONFIG_HOME", xdg.path())]);
        assert_nothing_outside_the_repository_leaked(&listed, "XDG_CONFIG_HOME put back");
        assert!(
            !listed.contains("leaked-from-xdg"),
            "$XDG_CONFIG_HOME/git/config is visible to the tests:\n{listed}"
        );
    }
}
