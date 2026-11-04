use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

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
/// worktree's own HEAD) and, when a branch is checked out, its ref. A packed ref has no file of its
/// own until the next commit writes one, so then the directory it will be created in is watched
/// (Cargo rescans watched directories), together with `packed-refs`. Only existing paths are
/// returned, because Cargo reruns the build script on every build for a missing one.
fn git_files_to_watch() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    paths.extend(git(&["rev-parse", "--git-path", "HEAD"]).map(PathBuf::from));
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(ref_path) = git(&["rev-parse", "--git-path", &branch]).map(PathBuf::from) {
            if ref_path.exists() {
                paths.push(ref_path);
            } else {
                let refs_root = git(&["rev-parse", "--git-path", "refs"]).map(PathBuf::from);
                let ref_dir = ref_path
                    .ancestors()
                    .skip(1)
                    .take_while(|dir| refs_root.as_ref().is_none_or(|root| dir.starts_with(root)))
                    .find(|dir| dir.is_dir());
                paths.extend(ref_dir.map(Path::to_path_buf));
            }
        }
        paths.extend(git(&["rev-parse", "--git-path", "packed-refs"]).map(PathBuf::from));
    }
    paths.retain(|path| path.exists());
    paths
}
