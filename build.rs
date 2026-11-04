use std::{env, path::PathBuf, process::Command};

/// Set by release builds that do not run from a git checkout (a source tarball, for example).
const GIT_HASH_ENV: &str = "GDRIVE_GIT_HASH";

fn main() {
    println!("cargo:rerun-if-env-changed={GIT_HASH_ENV}");
    for path in git_files_to_watch() {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    let git_hash = git(&["rev-parse", "HEAD"])
        .or_else(|| {
            env::var(GIT_HASH_ENV)
                .ok()
                .map(|hash| hash.trim().to_owned())
                .filter(|hash| !hash.is_empty())
        })
        .unwrap_or_else(|| String::from("unknown"));

    println!("cargo:rustc-env=GIT_HASH={git_hash}");
}

/// Runs git in the package directory and returns its trimmed output, or `None` when git is
/// missing, fails or prints nothing. `git rev-parse` also resolves a detached HEAD (a CI checkout
/// of a tag) and a worktree, where `.git` is a file rather than a directory.
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let value = stdout.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// The files that change when HEAD moves to another commit: HEAD itself (`.git/HEAD`, or the
/// worktree's own HEAD) and, when a branch is checked out, its ref. Only existing files are
/// returned, because Cargo reruns the build script on every build for a missing one.
fn git_files_to_watch() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    paths.extend(git(&["rev-parse", "--git-path", "HEAD"]));
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        paths.extend(git(&["rev-parse", "--git-path", &branch]));
        paths.extend(git(&["rev-parse", "--git-path", "packed-refs"]));
    }
    paths
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .collect()
}
