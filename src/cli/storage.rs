use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use fs2::FileExt;
use serde::Serialize;

use super::args::StorageCommand;

const BUILD_VERSION_MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const AUTOMATIC_SCRATCH_MIN_AGE: Duration = Duration::from_secs(72 * 60 * 60);
const AUTOMATIC_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

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
            include_clean_git,
            json,
        } => run_cleanup(
            apply,
            keep_builds,
            scratch_min_age_hours,
            include_clean_git,
            json,
        ),
    }
}

pub(crate) fn run_automatic_maintenance() {
    if matches!(
        std::env::var("JCODE_STORAGE_MAINTENANCE").as_deref(),
        Ok("0" | "false" | "no" | "off")
    ) {
        return;
    }
    if let Err(error) = run_automatic_maintenance_inner(SystemTime::now()) {
        crate::logging::warn(&format!("automatic storage maintenance skipped: {error:#}"));
    }
}

fn run_automatic_maintenance_inner(now: SystemTime) -> Result<usize> {
    let jcode = crate::storage::jcode_dir()?;
    crate::storage::ensure_dir(&jcode)?;
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(jcode.join("storage-maintenance.lock"))?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(0);
    }

    let stamp = jcode.join("storage-maintenance-at");
    if stamp
        .metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age < AUTOMATIC_MAINTENANCE_INTERVAL)
    {
        return Ok(0);
    }

    let removed = automatic_scratch_maintenance_in(&jcode, now)?;
    let timestamp = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    fs::write(&stamp, format!("{timestamp}\n"))?;
    Ok(removed)
}

fn automatic_scratch_maintenance_in(jcode: &Path, now: SystemTime) -> Result<usize> {
    let active_sessions = crate::session::session_presence()
        .into_iter()
        .map(|presence| presence.session_id)
        .collect::<HashSet<_>>();
    let default_scratch = jcode.join("scratch");
    let mut scratch_roots = vec![default_scratch.clone()];
    if let Some(configured) = std::env::var_os("JCODE_SCRATCH_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path != &default_scratch)
    {
        scratch_roots.push(configured);
    }
    let mut removed = 0usize;
    for scratch in scratch_roots {
        removed +=
            automatic_scratch_maintenance_with_active_sessions(&scratch, now, &active_sessions)?;
    }
    Ok(removed)
}

fn automatic_scratch_maintenance_with_active_sessions(
    scratch: &Path,
    now: SystemTime,
    active_sessions: &HashSet<String>,
) -> Result<usize> {
    let active_cwds = active_working_directories().unwrap_or_default();
    let Ok(entries) = fs::read_dir(scratch) else {
        return Ok(0);
    };
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let marker = path.join(".jcode-disposable");
        if !marker.is_file() {
            continue;
        }
        let owner = fs::read_to_string(&marker).unwrap_or_default();
        if active_sessions.contains(owner.trim()) {
            continue;
        }
        if scratch_path_deletable_bytes(&path, AUTOMATIC_SCRATCH_MIN_AGE, now, &active_cwds, false)
            .is_some()
        {
            candidates.push(path);
        }
    }

    let mut removed = 0usize;
    for path in candidates {
        if remove_scratch_candidate_if_still_safe_at(&path, AUTOMATIC_SCRATCH_MIN_AGE, false, now)?
        {
            removed += 1;
        }
    }
    Ok(removed)
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
    include_clean_git: bool,
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
        include_clean_git,
    )?;
    candidates.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));

    let candidate_bytes = candidates.iter().map(|item| item.bytes).sum();
    let mut reclaimed_bytes = 0u64;
    if apply {
        let mut removed = Vec::new();
        let mut skipped_after_revalidation = 0usize;
        for item in candidates {
            let did_remove = if item.kind == "build-version" {
                jcode_build_support::remove_unreferenced_build_version_in(
                    &jcode.join("builds"),
                    &item.path,
                    BUILD_VERSION_MIN_AGE,
                )?
            } else {
                remove_scratch_candidate_if_still_safe(
                    &item.path,
                    Duration::from_secs(scratch_min_age_hours.saturating_mul(3600)),
                    include_clean_git,
                )?
            };
            if did_remove {
                reclaimed_bytes = reclaimed_bytes.saturating_add(item.bytes);
                removed.push(item);
            } else {
                skipped_after_revalidation += 1;
            }
        }
        if skipped_after_revalidation > 0 {
            warnings.push(format!(
                "Skipped {skipped_after_revalidation} candidate(s) that became protected or changed before deletion"
            ));
        }
        candidates = removed;
    }
    let report = CleanupReport {
        applied: apply,
        reclaimed_bytes: if apply {
            reclaimed_bytes
        } else {
            candidate_bytes
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
        println!(
            "Re-run with `--apply` to recompute safety checks and delete candidates that remain eligible."
        );
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
    include_clean_git: bool,
) -> Result<Vec<CleanupItem>> {
    let mut items = Vec::new();
    if let Some(active_cwds) = active_cwds {
        items.extend(stale_scratch_candidates(
            &jcode.join("scratch"),
            scratch_min_age,
            now,
            active_cwds,
            include_clean_git,
        )?);
    }
    items.extend(old_build_candidates(
        &jcode.join("builds"),
        keep_builds,
        BUILD_VERSION_MIN_AGE,
        now,
    )?);
    Ok(items)
}

fn stale_scratch_candidates(
    scratch: &Path,
    min_age: Duration,
    now: SystemTime,
    active_cwds: &HashSet<PathBuf>,
    include_clean_git: bool,
) -> Result<Vec<CleanupItem>> {
    let Ok(entries) = fs::read_dir(scratch) else {
        return Ok(Vec::new());
    };
    let mut items = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(bytes) =
            scratch_path_deletable_bytes(&path, min_age, now, active_cwds, include_clean_git)
        else {
            continue;
        };
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

fn old_build_candidates(
    builds: &Path,
    keep_builds: usize,
    min_age: Duration,
    now: SystemTime,
) -> Result<Vec<CleanupItem>> {
    let versions = builds.join("versions");
    let Ok(entries) = fs::read_dir(&versions) else {
        return Ok(Vec::new());
    };
    let protected = jcode_build_support::protected_build_versions_in(builds)?;
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
        .filter_map(|(modified, path)| {
            now.duration_since(modified)
                .ok()
                .filter(|age| *age >= min_age)
                .map(|_| CleanupItem {
                    kind: "build-version",
                    bytes: path_size(&path).unwrap_or(0),
                    path,
                    reason: format!(
                        "unreferenced, at least {}h old, and older than the newest {keep_builds}",
                        min_age.as_secs() / 3600
                    ),
                })
        })
        .collect())
}

fn remove_scratch_candidate_if_still_safe(
    path: &Path,
    min_age: Duration,
    include_clean_git: bool,
) -> Result<bool> {
    remove_scratch_candidate_if_still_safe_at(path, min_age, include_clean_git, SystemTime::now())
}

fn remove_scratch_candidate_if_still_safe_at(
    path: &Path,
    min_age: Duration,
    include_clean_git: bool,
    now: SystemTime,
) -> Result<bool> {
    if scratch_owner_is_active(path) {
        return Ok(false);
    }
    let Some(active_cwds) = active_working_directories() else {
        // Managed disposable scratch still has owner-session and age guards on
        // platforms where enumerating every process CWD is unavailable.
        if !path.join(".jcode-disposable").is_file() {
            return Ok(false);
        }
        return remove_scratch_candidate_without_active_cwds(path, min_age, include_clean_git, now);
    };
    if scratch_path_deletable_bytes(path, min_age, now, &active_cwds, include_clean_git).is_none() {
        return Ok(false);
    }

    let Some(parent) = path.parent() else {
        return Ok(false);
    };
    let quarantine = (0..100).find_map(|attempt| {
        let candidate = parent.join(format!(".jcode-cleanup-{}-{attempt}", std::process::id()));
        (!candidate.exists()).then_some(candidate)
    });
    let Some(quarantine) = quarantine else {
        return Ok(false);
    };
    fs::rename(path, &quarantine)
        .with_context(|| format!("failed to quarantine {}", path.display()))?;

    let safe_after_rename = !scratch_owner_is_active(&quarantine)
        && active_working_directories().is_some_and(|active_cwds| {
            scratch_path_deletable_bytes(&quarantine, min_age, now, &active_cwds, include_clean_git)
                .is_some()
        });
    if !safe_after_rename {
        fs::rename(&quarantine, path).with_context(|| {
            format!(
                "failed to restore protected scratch candidate {}",
                path.display()
            )
        })?;
        return Ok(false);
    }
    remove_path(&quarantine).with_context(|| {
        format!(
            "failed to remove quarantined scratch path {}",
            path.display()
        )
    })?;
    Ok(true)
}

fn remove_scratch_candidate_without_active_cwds(
    path: &Path,
    min_age: Duration,
    include_clean_git: bool,
    now: SystemTime,
) -> Result<bool> {
    let empty = HashSet::new();
    if scratch_path_deletable_bytes(path, min_age, now, &empty, include_clean_git).is_none() {
        return Ok(false);
    }
    let Some(parent) = path.parent() else {
        return Ok(false);
    };
    let quarantine = parent.join(format!(".jcode-cleanup-{}-fallback", std::process::id()));
    if quarantine.exists() {
        return Ok(false);
    }
    fs::rename(path, &quarantine)
        .with_context(|| format!("failed to quarantine {}", path.display()))?;
    let safe = !scratch_owner_is_active(&quarantine)
        && scratch_path_deletable_bytes(&quarantine, min_age, now, &empty, include_clean_git)
            .is_some();
    if !safe {
        fs::rename(&quarantine, path).with_context(|| {
            format!(
                "failed to restore protected scratch candidate {}",
                path.display()
            )
        })?;
        return Ok(false);
    }
    remove_path(&quarantine).with_context(|| {
        format!(
            "failed to remove quarantined scratch path {}",
            path.display()
        )
    })?;
    Ok(true)
}

fn scratch_owner_is_active(path: &Path) -> bool {
    let Ok(owner) = fs::read_to_string(path.join(".jcode-disposable")) else {
        return false;
    };
    let owner = owner.trim();
    !owner.is_empty()
        && crate::session::session_presence()
            .into_iter()
            .any(|presence| presence.session_id == owner)
}

fn scratch_path_deletable_bytes(
    path: &Path,
    min_age: Duration,
    now: SystemTime,
    active_cwds: &HashSet<PathBuf>,
    include_clean_git: bool,
) -> Option<u64> {
    if path.join(".jcode-keep").exists()
        || path_is_active(path, &active_cwds)
        || scratch_git_worktree_needs_preservation(
            path,
            include_clean_git || path.join(".jcode-disposable").exists(),
        )
    {
        return None;
    }
    path_stats(path)
        .ok()
        .and_then(|(bytes, modified)| now.duration_since(modified).ok().map(|age| (bytes, age)))
        .filter(|(_, age)| *age >= min_age)
        .map(|(bytes, _)| bytes)
}

fn path_is_active(path: &Path, active_cwds: &HashSet<PathBuf>) -> bool {
    active_cwds
        .iter()
        .any(|cwd| cwd == path || cwd.starts_with(path))
}

fn scratch_git_worktree_needs_preservation(path: &Path, include_clean_git: bool) -> bool {
    let Some(worktrees) = nested_git_worktrees(path, include_clean_git) else {
        return true;
    };
    if worktrees.is_empty() {
        return false;
    }
    if !include_clean_git {
        return true;
    }
    for worktree in worktrees {
        let output = std::process::Command::new("git")
            .args([
                "-C",
                worktree.to_string_lossy().as_ref(),
                "status",
                "--porcelain",
            ])
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output();
        if !matches!(output, Ok(output) if output.status.success() && output.stdout.is_empty()) {
            return true;
        }
    }
    false
}

fn nested_git_worktrees(path: &Path, collect_all: bool) -> Option<Vec<PathBuf>> {
    let mut worktrees = Vec::new();
    let mut pending = vec![path.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = fs::read_dir(&dir).ok()?;
        for entry in entries {
            let entry = entry.ok()?;
            let child = entry.path();
            if entry.file_name() == ".git" {
                worktrees.push(dir.clone());
                if !collect_all {
                    return Some(worktrees);
                }
                continue;
            }
            let file_type = entry.file_type().ok()?;
            if file_type.is_dir() && !file_type.is_symlink() {
                pending.push(child);
            }
        }
    }
    Some(worktrees)
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
        for name in [
            "stable",
            "current",
            "pending-new",
            "old-a",
            "old-b",
            "newest",
        ] {
            write_bytes(&builds.join("versions").join(name).join("jcode"), 16);
        }
        fs::write(builds.join("stable-version"), "stable\n").unwrap();
        fs::write(builds.join("current-version"), "current\n").unwrap();
        fs::write(
            builds.join("manifest.json"),
            serde_json::to_vec(&jcode_build_support::BuildManifest {
                pending_activation: Some(jcode_build_support::PendingActivation {
                    session_id: "session-test".to_string(),
                    new_version: "pending-new".to_string(),
                    previous_current_version: None,
                    previous_shared_server_version: None,
                    source_fingerprint: None,
                    requested_at: chrono::Utc::now(),
                }),
                ..jcode_build_support::BuildManifest::default()
            })
            .unwrap(),
        )
        .unwrap();
        let mut candidates = old_build_candidates(
            &builds,
            1,
            BUILD_VERSION_MIN_AGE,
            SystemTime::now() + Duration::from_secs(48 * 3600),
        )
        .unwrap();
        candidates.sort_by(|a, b| a.path.cmp(&b.path));
        let names: Vec<_> = candidates
            .iter()
            .filter_map(|item| item.path.file_name().and_then(|name| name.to_str()))
            .collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"old-a") || names.contains(&"old-b"));
        assert!(!names.contains(&"stable"));
        assert!(!names.contains(&"current"));
        assert!(!names.contains(&"pending-new"));
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
            false,
        )
        .unwrap();
        assert!(recent.is_empty());

        let future = SystemTime::now() + Duration::from_secs(48 * 3600);
        let candidates = stale_scratch_candidates(
            &scratch,
            Duration::from_secs(24 * 3600),
            future,
            &active_cwds,
            false,
        )
        .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].path, stale);
        assert_eq!(candidates[0].bytes, 32);
    }

    #[test]
    fn scratch_apply_revalidation_honors_new_protection_and_modifications() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("candidate");
        write_bytes(&path.join("artifact"), 32);
        let future = SystemTime::now() + Duration::from_secs(48 * 3600);
        let active_cwds = HashSet::new();

        assert_eq!(
            scratch_path_deletable_bytes(
                &path,
                Duration::from_secs(24 * 3600),
                future,
                &active_cwds,
                false,
            ),
            Some(32)
        );

        fs::write(path.join(".jcode-keep"), "").unwrap();
        assert_eq!(
            scratch_path_deletable_bytes(
                &path,
                Duration::from_secs(24 * 3600),
                future,
                &active_cwds,
                false,
            ),
            None
        );
        fs::remove_file(path.join(".jcode-keep")).unwrap();

        assert_eq!(
            scratch_path_deletable_bytes(
                &path,
                Duration::from_secs(24 * 3600),
                SystemTime::now(),
                &active_cwds,
                false,
            ),
            None
        );
    }

    #[test]
    fn automatic_maintenance_only_retires_stale_managed_scratch() {
        let temp = tempfile::tempdir().unwrap();
        let jcode = temp.path();
        let scratch = jcode.join("scratch");
        let managed_stale = scratch.join("session-stale");
        let managed_recent = scratch.join("session-recent");
        let managed_active = scratch.join("session-active");
        let unmanaged_stale = scratch.join("legacy-unmanaged");
        for path in [
            &managed_stale,
            &managed_recent,
            &managed_active,
            &unmanaged_stale,
        ] {
            write_bytes(&path.join("artifact"), 32);
        }
        fs::write(managed_stale.join(".jcode-disposable"), "stale-session\n").unwrap();
        fs::write(managed_recent.join(".jcode-disposable"), "recent-session\n").unwrap();
        fs::write(managed_active.join(".jcode-disposable"), "active-session\n").unwrap();

        let now = SystemTime::now();
        let future = now + Duration::from_secs(96 * 3600);
        write_bytes(&managed_recent.join("recent"), 1);
        fs::File::options()
            .write(true)
            .open(managed_recent.join("recent"))
            .unwrap()
            .set_modified(future - Duration::from_secs(3600))
            .unwrap();
        let removed = automatic_scratch_maintenance_with_active_sessions(
            &scratch,
            future,
            &HashSet::from(["active-session".to_string()]),
        )
        .unwrap();

        assert_eq!(removed, 1);
        assert!(!managed_stale.exists());
        assert!(managed_recent.exists());
        assert!(managed_active.exists());
        assert!(unmanaged_stale.exists());
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
            false,
        )
        .unwrap();
        assert!(candidates.is_empty());
    }

    #[test]
    fn scratch_cleanup_preserves_git_worktrees_with_uncommitted_changes() {
        let temp = tempfile::tempdir().unwrap();
        let scratch = temp.path().join("scratch");
        let root = scratch.join("outer");
        let checkout = root.join("nested");
        fs::create_dir_all(&checkout).unwrap();
        for args in [
            vec!["init"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test User"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&checkout)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        fs::write(checkout.join("tracked"), "clean").unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["add", "tracked"])
                .current_dir(&checkout)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args(["commit", "-m", "fixture"])
                .current_dir(&checkout)
                .status()
                .unwrap()
                .success()
        );
        fs::write(checkout.join("tracked"), "modified").unwrap();

        let candidates = stale_scratch_candidates(
            &scratch,
            Duration::ZERO,
            SystemTime::now() + Duration::from_secs(1),
            &HashSet::new(),
            true,
        )
        .unwrap();
        assert!(candidates.is_empty());

        assert!(
            std::process::Command::new("git")
                .args(["checkout", "--", "tracked"])
                .current_dir(&checkout)
                .status()
                .unwrap()
                .success()
        );
        let default_clean_candidates = stale_scratch_candidates(
            &scratch,
            Duration::ZERO,
            SystemTime::now() + Duration::from_secs(1),
            &HashSet::new(),
            false,
        )
        .unwrap();
        assert!(default_clean_candidates.is_empty());

        let clean_candidates = stale_scratch_candidates(
            &scratch,
            Duration::ZERO,
            SystemTime::now() + Duration::from_secs(1),
            &HashSet::new(),
            true,
        )
        .unwrap();
        assert_eq!(clean_candidates.len(), 1);
        assert_eq!(clean_candidates[0].path, root);
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
