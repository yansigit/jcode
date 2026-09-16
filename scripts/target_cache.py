#!/usr/bin/env python3
"""Run Cargo under a shared target lock and prune stale cache generations safely."""

from __future__ import annotations

import argparse
import fcntl
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time

HASHED_ARTIFACT = re.compile(r"^(?P<base>.+)-(?P<hash>[0-9a-f]{16,})(?P<ext>\.[^.]+)?$")
INCREMENTAL_GENERATION = re.compile(r"^(?P<base>.+)-[0-9a-z]+$")


def open_lock(path: Path):
    path.parent.mkdir(parents=True, exist_ok=True)
    handle = path.open("a+")
    return handle


def run_locked(args: argparse.Namespace) -> int:
    if not args.command:
        raise SystemExit("target_cache.py run requires a command after --")
    command = args.command[1:] if args.command[0] == "--" else args.command
    with open_lock(args.lock) as lock:
        fcntl.flock(lock.fileno(), fcntl.LOCK_SH)
        return subprocess.call(command)


def path_stats(path: Path) -> tuple[int, float]:
    try:
        stat = path.lstat()
    except OSError:
        return 0, 0.0
    if path.is_symlink() or path.is_file():
        return stat.st_size, stat.st_mtime
    if not path.is_dir():
        return 0, stat.st_mtime
    total = 0
    latest = stat.st_mtime
    for root, dirs, files in os.walk(path, followlinks=False):
        root_path = Path(root)
        kept_dirs = []
        for name in dirs:
            child = root_path / name
            try:
                child_stat = child.lstat()
            except OSError:
                continue
            latest = max(latest, child_stat.st_mtime)
            if child.is_symlink():
                total += child_stat.st_size
            else:
                kept_dirs.append(name)
        dirs[:] = kept_dirs
        for name in files:
            child = root_path / name
            try:
                child_stat = child.lstat()
            except OSError:
                continue
            total += child_stat.st_size
            latest = max(latest, child_stat.st_mtime)
    return total, latest


def remove_path(path: Path) -> bool:
    try:
        if path.is_symlink() or path.is_file():
            path.unlink()
        elif path.is_dir():
            shutil.rmtree(path)
        else:
            return False
        return True
    except OSError as error:
        print(f"target_cache: failed to remove {path}: {error}", file=sys.stderr)
        return False


def stale_candidates(target: Path, cutoff: float) -> list[tuple[float, int, Path, str]]:
    candidates: list[tuple[float, int, Path, str]] = []
    for profile_name in ("debug", "selfdev", "release"):
        profile = target / profile_name
        incremental = profile / "incremental"
        if incremental.is_dir():
            for child in incremental.iterdir():
                if not child.is_dir() or child.is_symlink():
                    continue
                size, latest = path_stats(child)
                if latest < cutoff:
                    candidates.append((latest, size, child, "stale incremental session"))

        deps = profile / "deps"
        if not deps.is_dir():
            continue
        generations: dict[str, list[tuple[float, int, Path]]] = {}
        for child in deps.iterdir():
            if not child.is_file() or child.is_symlink():
                continue
            match = HASHED_ARTIFACT.match(child.name)
            if not match:
                continue
            stat = child.stat()
            key = f"{match.group('base')}{match.group('ext') or ''}"
            generations.setdefault(key, []).append((stat.st_mtime, stat.st_size, child))
        for artifacts in generations.values():
            artifacts.sort(reverse=True)
            for modified, size, child in artifacts[1:]:
                if modified < cutoff:
                    candidates.append((modified, size, child, "stale dependency generation"))
    candidates.sort(key=lambda candidate: candidate[0])
    return candidates


def surplus_generation_candidates(
    target: Path, keep_generations: int
) -> list[tuple[float, int, Path, str]]:
    """Return old cache generations while retaining a small warm working set.

    Cargo creates a new fingerprinted generation whenever features, compiler
    flags, or the selected toolchain change. Those generations otherwise live
    forever. Once the target directory is above its soft limit, age alone is a
    poor bound because parallel development can create tens of GiB in one day.
    Keep the newest few generations per logical artifact and retire the rest.
    Missing Cargo cache entries are safe: Cargo regenerates them on demand.
    """
    candidates: list[tuple[float, int, Path, str]] = []
    for profile_name in ("debug", "selfdev", "release"):
        profile = target / profile_name
        incremental = profile / "incremental"
        incremental_groups: dict[str, list[tuple[float, int, Path]]] = {}
        if incremental.is_dir():
            for child in incremental.iterdir():
                if not child.is_dir() or child.is_symlink():
                    continue
                match = INCREMENTAL_GENERATION.match(child.name)
                if not match:
                    continue
                size, latest = path_stats(child)
                incremental_groups.setdefault(match.group("base"), []).append(
                    (latest, size, child)
                )
        for generations in incremental_groups.values():
            generations.sort(reverse=True)
            for modified, size, child in generations[keep_generations:]:
                candidates.append(
                    (modified, size, child, "surplus incremental generation")
                )

        deps = profile / "deps"
        dependency_groups: dict[str, list[tuple[float, int, Path]]] = {}
        if deps.is_dir():
            for child in deps.iterdir():
                if not child.is_file() or child.is_symlink():
                    continue
                match = HASHED_ARTIFACT.match(child.name)
                if not match:
                    continue
                stat = child.stat()
                key = f"{match.group('base')}{match.group('ext') or ''}"
                dependency_groups.setdefault(key, []).append(
                    (stat.st_mtime, stat.st_size, child)
                )
        for generations in dependency_groups.values():
            generations.sort(reverse=True)
            for modified, size, child in generations[keep_generations:]:
                candidates.append(
                    (modified, size, child, "surplus dependency generation")
                )

        # Recent nightly Cargo versions use build/<crate>/<fingerprint> as a
        # disk-backed compiler cache. This is distinct from the traditional
        # debug/build/<crate>-<hash> build-script layout, so only prune the
        # nested form we can group unambiguously.
        build_cache = profile / "build"
        if build_cache.is_dir():
            for crate_dir in build_cache.iterdir():
                if not crate_dir.is_dir() or crate_dir.is_symlink():
                    continue
                generations = []
                for child in crate_dir.iterdir():
                    if child.is_dir() and not child.is_symlink():
                        size, latest = path_stats(child)
                        generations.append((latest, size, child))
                generations.sort(reverse=True)
                for modified, size, child in generations[keep_generations:]:
                    candidates.append(
                        (modified, size, child, "surplus compiler-cache generation")
                    )

    candidates.sort(key=lambda candidate: candidate[0])
    return candidates


def prune_locked(args: argparse.Namespace) -> int:
    target = args.target.resolve()
    target.mkdir(parents=True, exist_ok=True)
    with open_lock(args.lock) as lock:
        try:
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return 0

        stamp = target / ".jcode-cache-maintenance-at"
        now = time.time()
        try:
            if now - stamp.stat().st_mtime < args.interval_hours * 3600:
                return 0
        except OSError:
            pass

        total, _ = path_stats(target)
        limit = int(args.max_gib * 1024**3)
        if total <= limit:
            stamp.write_text(f"{int(now)}\n")
            return 0

        cutoff = now - args.min_age_days * 86400
        reclaimed = 0
        removed = 0
        seen: set[Path] = set()
        candidates = stale_candidates(target, cutoff)
        candidates.extend(surplus_generation_candidates(target, args.keep_generations))
        candidates.sort(key=lambda candidate: candidate[0])
        for _, size, path, reason in candidates:
            if total - reclaimed <= limit:
                break
            if path in seen or not path.exists():
                continue
            seen.add(path)
            if remove_path(path):
                reclaimed += size
                removed += 1
                print(
                    f"target_cache: removed {reason}: {path} ({size / 1024**2:.1f} MiB)",
                    file=sys.stderr,
                )
        stamp.write_text(f"{int(now)}\n")
        remaining = max(0, total - reclaimed)
        print(
            f"target_cache: maintenance removed {removed} stale item(s), reclaimed "
            f"{reclaimed / 1024**3:.2f} GiB; estimated target size {remaining / 1024**3:.2f} GiB",
            file=sys.stderr,
        )
        if remaining > limit:
            print(
                "target_cache: target remains above the soft limit because no more old cache "
                "generations were safe to remove; run scripts/clean_target.sh for deeper cleanup",
                file=sys.stderr,
            )
    return 0


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser()
    subcommands = root.add_subparsers(dest="action", required=True)

    run = subcommands.add_parser("run")
    run.add_argument("--lock", type=Path, required=True)
    run.add_argument("command", nargs=argparse.REMAINDER)
    run.set_defaults(handler=run_locked)

    prune = subcommands.add_parser("prune")
    prune.add_argument("--lock", type=Path, required=True)
    prune.add_argument("--target", type=Path, required=True)
    prune.add_argument("--max-gib", type=float, default=30.0)
    prune.add_argument("--min-age-days", type=float, default=7.0)
    prune.add_argument("--interval-hours", type=float, default=24.0)
    prune.add_argument("--keep-generations", type=int, default=3)
    prune.set_defaults(handler=prune_locked)
    return root


def main() -> int:
    args = parser().parse_args()
    return args.handler(args)


if __name__ == "__main__":
    raise SystemExit(main())
