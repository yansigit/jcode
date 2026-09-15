use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::Serialize;

use super::args::StorageCommand;

#[derive(Debug, Serialize)]
struct StorageEntry {
    path: PathBuf,
    bytes: u64,
}

#[derive(Debug, Serialize)]
struct StorageStatus {
    jcode: StorageEntry,
    scratch: StorageEntry,
    build_versions: StorageEntry,
    project_target: Option<StorageEntry>,
}

#[derive(Debug, Clone, Serialize)]
struct CleanupItem {
    kind: &'static str,
    path: PathBuf,
    bytes: u64,
    reason: String,
}

#[derive(Debug, Serialize)]
struct CleanupReport {
    applied: bool,
    reclaimed_bytes: u64,
    candidates: Vec<CleanupItem>,
    warnings: Vec<String>,
}

pub(crate) fn run(action: StorageCommand) -> Result<()> {
    match action {
        StorageCommand::Status { json } => run_status(json),
        StorageCommand::Cleanup {
            apply,
            keep_builds,
            scratch_min_age_hours,
            json,
        } => run_cleanup(apply, keep_builds, scratch_min_age_hours, json),
    }
}

fn run_status(json: bool) -> Result<()> {
    let jcode = crate::storage::jcode_dir()?;
    let scratch = jcode.join("scratch");
    let versions = jcode.join("builds/versions");
    let project_target = nearest_project_target();
    let status = StorageStatus {
        jcode: storage_entry(&jcode),
        scratch: storage_entry(&scratch),
        build_versions: storage_entry(&versions),
        project_target: project_target.as_deref().map(storage_entry),
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }

    println!("Jcode storage");
    print_entry("managed data", &status.jcode);
    print_entry("scratch", &status.scratch);
    print_entry("build versions", &status.build_versions);
    if let Some(target) = &status.project_target {
        print_entry("project target", target);
    }
    println!();
    println!("Run `jcode storage cleanup` for a safe dry run.");
    println!("Add `--apply` only after reviewing the candidate list.");
    Ok(())
}

fn run_cleanup(
    apply: bool,
    keep_builds: usize,
    scratch_min_age_hours: u64,
    json: bool,
) -> Result<()> {
    let jcode = crate::storage::jcode_dir()?;
    let active_cwds = active_working_directories();
    let mut warnings = Vec::new();
    if active_cwds.is_none() {
        warnings.push(
            "Could not inspect active process working directories; scratch cleanup was skipped"
                .to_string(),
        );
    }
    let mut candidates = cleanup_candidates(
        &jcode,
        keep_builds,
        Duration::from_secs(scratch_min_age_hours.saturating_mul(3600)),
        SystemTime::now(),
        active_cwds.as_ref(),
    )?;
    candidates.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));

    let mut reclaimed_bytes = 0u64;
    if apply {
        for item in &candidates {
            remove_path(&item.path)
                .with_context(|| format!("failed to remove {}", item.path.display()))?;
            reclaimed_bytes = reclaimed_bytes.saturating_add(item.bytes);
        }
    }
    let report = CleanupReport {
        applied: apply,
        reclaimed_bytes: if apply {
            reclaimed_bytes
        } else {
            candidates.iter().map(|item| item.bytes).sum()
        },
        candidates,
        warnings,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!(
        "Storage cleanup {}",
        if apply { "completed" } else { "dry run" }
    );
    for item in &report.candidates {
        println!(
            "  {:>9}  {:<13} {} ({})",
            human_bytes(item.bytes),
            item.kind,
            item.path.display(),
            item.reason
        );
    }
    for warning in &report.warnings {
        eprintln!("Warning: {warning}");
    }
    println!(
        "{} {} across {} item(s).",
        if apply { "Reclaimed" } else { "Would reclaim" },
        human_bytes(report.reclaimed_bytes),
        report.candidates.len()
    );
    if !apply && !report.candidates.is_empty() {
        println!("Re-run with `--apply` to delete exactly these candidates.");
    }
    Ok(())
}

fn storage_entry(path: &Path) -> StorageEntry {
    StorageEntry {
        path: path.to_path_buf(),
        bytes: path_size(path).unwrap_or(0),
    }
}

fn print_entry(label: &str, entry: &StorageEntry) {
    println!(
        "  {:<16} {:>9}  {}",
        label,
        human_bytes(entry.bytes),
        entry.path.display()
    );
}

fn cleanup_candidates(
    jcode: &Path,
    keep_builds: usize,
    scratch_min_age: Duration,
    now: SystemTime,
    active_cwds: Option<&HashSet<PathBuf>>,
) -> Result<Vec<CleanupItem>> {
    let mut items = Vec::new();
    if let Some(active_cwds) = active_cwds {
        items.extend(stale_scratch_candidates(
            &jcode.join("scratch"),
            scratch_min_age,
            now,
            active_cwds,
        )?);
    }
    items.extend(old_build_candidates(&jcode.join("builds"), keep_builds)?);
    Ok(items)
}

fn stale_scratch_candidates(
    scratch: &Path,
    min_age: Duration,
    now: SystemTime,
    active_cwds: &HashSet<PathBuf>,
) -> Result<Vec<CleanupItem>> {
    let Ok(entries) = fs::read_dir(scratch) else {
        return Ok(Vec::new());
    };
    let mut items = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.join(".jcode-keep").exists() || path_is_active(&path, active_cwds) {
            continue;
        }
        let Ok((bytes, modified)) = path_stats(&path) else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age < min_age {
            continue;
        }
        items.push(CleanupItem {
            kind: "scratch",
            bytes,
            path,
            reason: format!(
                "no files modified for at least {}h",
                min_age.as_secs() / 3600
            ),
        });
    }
    Ok(items)
}

fn old_build_candidates(builds: &Path, keep_builds: usize) -> Result<Vec<CleanupItem>> {
    let versions = builds.join("versions");
    let Ok(entries) = fs::read_dir(&versions) else {
        return Ok(Vec::new());
    };
    let protected = protected_build_versions(builds);
    let mut unprotected = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if protected.contains(name) {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        unprotected.push((modified, path));
    }
    unprotected.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(unprotected
        .into_iter()
        .skip(keep_builds)
        .map(|(_, path)| CleanupItem {
            kind: "build-version",
            bytes: path_size(&path).unwrap_or(0),
            path,
            reason: format!("unreferenced and older than the newest {keep_builds}"),
        })
        .collect())
}

fn protected_build_versions(builds: &Path) -> HashSet<String> {
    let mut protected = HashSet::new();
    for marker in [
        "current-version",
        "stable-version",
        "shared-server-version",
        "canary-version",
    ] {
        if let Ok(value) = fs::read_to_string(builds.join(marker)) {
            let value = value.trim();
            if !value.is_empty() {
                protected.insert(value.to_string());
            }
        }
    }
    for channel in ["current", "stable", "shared-server", "canary"] {
        let binary = builds.join(channel).join(jcode_binary_name());
        if let Ok(target) = binary.canonicalize()
            && let Some(name) = target
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
        {
            protected.insert(name.to_string());
        }
    }
    if let Ok(exe) = std::env::current_exe()
        && let Ok(exe) = exe.canonicalize()
        && let Some(name) = exe
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
    {
        protected.insert(name.to_string());
    }
    protected
}

fn jcode_binary_name() -> &'static str {
    if cfg!(windows) { "jcode.exe" } else { "jcode" }
}

fn path_is_active(path: &Path, active_cwds: &HashSet<PathBuf>) -> bool {
    active_cwds
        .iter()
        .any(|cwd| cwd == path || cwd.starts_with(path))
}

fn remove_path(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn path_size(path: &Path) -> io::Result<u64> {
    path_stats(path).map(|(bytes, _)| bytes)
}

fn path_stats(path: &Path) -> io::Result<(u64, SystemTime)> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok((0, metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
    }
    if metadata.is_file() {
        return Ok((
            metadata.len(),
            metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        ));
    }
    if !metadata.is_dir() {
        return Ok((0, metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
    }
    let mut total = 0u64;
    let mut latest = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let (bytes, modified) = path_stats(&entry.path())?;
        total = total.saturating_add(bytes);
        latest = latest.max(modified);
    }
    Ok((total, latest))
}

fn nearest_project_target() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    for ancestor in cwd.ancestors() {
        if ancestor.join("Cargo.toml").is_file() {
            let target = ancestor.join("target");
            return target.exists().then_some(target);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn active_working_directories() -> Option<HashSet<PathBuf>> {
    let mut paths = HashSet::new();
    for entry in fs::read_dir("/proc").ok()?.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .chars()
            .all(|c| c.is_ascii_digit())
            && let Ok(path) = fs::read_link(entry.path().join("cwd"))
        {
            paths.insert(path);
        }
    }
    Some(paths)
}

#[cfg(target_os = "macos")]
fn active_working_directories() -> Option<HashSet<PathBuf>> {
    let output = std::process::Command::new("lsof")
        .args(["-n", "-a", "-d", "cwd", "-F", "n"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let paths = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix('n'))
        .map(PathBuf::from)
        .collect();
    Some(paths)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn active_working_directories() -> Option<HashSet<PathBuf>> {
    None
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_bytes(path: &Path, count: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = fs::File::create(path).unwrap();
        file.write_all(&vec![b'x'; count]).unwrap();
    }

    #[test]
    fn build_cleanup_keeps_channels_and_requested_unreferenced_count() {
        let temp = tempfile::tempdir().unwrap();
        let builds = temp.path().join("builds");
        for name in ["stable", "current", "old-a", "old-b", "newest"] {
            write_bytes(&builds.join("versions").join(name).join("jcode"), 16);
        }
        fs::write(builds.join("stable-version"), "stable\n").unwrap();
        fs::write(builds.join("current-version"), "current\n").unwrap();
        let mut candidates = old_build_candidates(&builds, 1).unwrap();
        candidates.sort_by(|a, b| a.path.cmp(&b.path));
        let names: Vec<_> = candidates
            .iter()
            .filter_map(|item| item.path.file_name().and_then(|name| name.to_str()))
            .collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"old-a") || names.contains(&"old-b"));
        assert!(!names.contains(&"stable"));
        assert!(!names.contains(&"current"));
    }

    #[test]
    fn scratch_cleanup_skips_active_recent_and_keep_marked_paths() {
        let temp = tempfile::tempdir().unwrap();
        let scratch = temp.path().join("scratch");
        let stale = scratch.join("stale");
        let active = scratch.join("active");
        let kept = scratch.join("kept");
        write_bytes(&stale.join("artifact"), 32);
        write_bytes(&active.join("artifact"), 32);
        write_bytes(&kept.join("artifact"), 32);
        fs::write(kept.join(".jcode-keep"), "").unwrap();
        let active_cwds = HashSet::from([active.join("nested")]);
        let recent = stale_scratch_candidates(
            &scratch,
            Duration::from_secs(24 * 3600),
            SystemTime::now(),
            &active_cwds,
        )
        .unwrap();
        assert!(recent.is_empty());

        let future = SystemTime::now() + Duration::from_secs(48 * 3600);
        let candidates = stale_scratch_candidates(
            &scratch,
            Duration::from_secs(24 * 3600),
            future,
            &active_cwds,
        )
        .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].path, stale);
        assert_eq!(candidates[0].bytes, 32);
    }

    #[test]
    fn scratch_age_uses_newest_nested_modification() {
        let temp = tempfile::tempdir().unwrap();
        let scratch = temp.path().join("scratch");
        let checkout = scratch.join("checkout");
        write_bytes(&checkout.join("old-artifact"), 32);
        let before_update = SystemTime::now();
        write_bytes(&checkout.join("target/recent-artifact"), 16);
        let candidates = stale_scratch_candidates(
            &scratch,
            Duration::from_secs(2),
            before_update + Duration::from_secs(1),
            &HashSet::new(),
        )
        .unwrap();
        assert!(candidates.is_empty());
    }

    #[test]
    fn path_size_does_not_follow_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        write_bytes(&temp.path().join("data/file"), 64);
        #[cfg(unix)]
        std::os::unix::fs::symlink(temp.path().join("data"), temp.path().join("link")).unwrap();
        assert_eq!(path_size(temp.path()).unwrap(), 64);
    }
}
