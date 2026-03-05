use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set");
    let repo_root = Path::new(&manifest_dir).join("../..");

    configure_git_rerun_inputs(&repo_root);

    let describe = git_output(&repo_root, &["describe", "--tags", "--always"]);
    let repo_slug = resolve_repo_slug(&repo_root);
    let version = format_version(repo_slug.as_deref(), describe.as_deref());

    println!("cargo:rustc-env=CODEX_CLI_VERSION={version}");
}

fn format_version(repo_slug: Option<&str>, describe: Option<&str>) -> String {
    match (repo_slug, describe) {
        (Some(repo), Some(desc)) => format!("{repo} {desc}"),
        (Some(repo), None) => repo.to_string(),
        (None, Some(desc)) => desc.to_string(),
        (None, None) => "unknown".to_string(),
    }
}

fn git_output(repo_root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8(output.stdout).ok()?;
    let value = text.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn parse_repo_slug(remote_url: &str) -> Option<String> {
    let trimmed = remote_url.trim().trim_end_matches('/');

    if let Some((_, rest)) = trimmed.split_once("github.com/") {
        return normalize_slug(rest);
    }
    if let Some((_, rest)) = trimmed.split_once("github.com:") {
        return normalize_slug(rest);
    }

    None
}

fn resolve_repo_slug(repo_root: &Path) -> Option<String> {
    if let Some(origin_url) = git_output(repo_root, &["remote", "get-url", "origin"])
        && let Some(slug) = parse_repo_slug(&origin_url)
    {
        return Some(slug);
    }

    let remote_name = git_output(repo_root, &["remote"])?;
    let first_remote = remote_name.lines().next()?.trim();
    if first_remote.is_empty() {
        return None;
    }
    let remote_url = git_output(repo_root, &["remote", "get-url", first_remote])?;
    parse_repo_slug(&remote_url)
}

fn normalize_slug(path: &str) -> Option<String> {
    let without_git_suffix = path.trim_start_matches('/').trim_end_matches(".git");
    let mut parts = without_git_suffix
        .split('/')
        .filter(|part| !part.is_empty());
    let owner = parts.next()?;
    let repo = parts.next()?;
    Some(format!("{owner}/{repo}"))
}

fn configure_git_rerun_inputs(repo_root: &Path) {
    let git_path = repo_root.join(".git");
    if git_path.is_dir() {
        track_git_dir(&git_path);
        return;
    }

    if !git_path.is_file() {
        return;
    }

    let Ok(contents) = fs::read_to_string(&git_path) else {
        return;
    };
    let Some(git_dir) = contents.trim().strip_prefix("gitdir: ") else {
        return;
    };
    let git_dir_path = if Path::new(git_dir).is_absolute() {
        PathBuf::from(git_dir)
    } else {
        repo_root.join(git_dir)
    };
    track_git_dir(&git_dir_path);
}

fn track_git_dir(git_dir: &Path) {
    let head = git_dir.join("HEAD");
    println!("cargo:rerun-if-changed={}", head.display());
    println!(
        "cargo:rerun-if-changed={}",
        git_dir.join("packed-refs").display()
    );

    let Ok(contents) = fs::read_to_string(&head) else {
        return;
    };
    let Some(reference) = contents.trim().strip_prefix("ref: ") else {
        return;
    };
    println!(
        "cargo:rerun-if-changed={}",
        git_dir.join(reference.trim()).display()
    );
}
