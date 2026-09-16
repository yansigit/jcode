#!/usr/bin/env python3

from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest


SCRIPT = Path(__file__).with_name("target_cache.py")
DEV_CARGO = Path(__file__).with_name("dev_cargo.sh")


def write_sized(path: Path, size: int, modified: float) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(b"x" * size)
    os.utime(path, (modified, modified))


def run_prune(root: Path, *, interval_hours: float = 0.0) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            "prune",
            "--lock",
            str(root / "target.lock"),
            "--target",
            str(root / "target"),
            "--max-gib",
            "0.000001",
            "--min-age-days",
            "1",
            "--interval-hours",
            str(interval_hours),
            "--keep-generations",
            "2",
        ],
        check=True,
        text=True,
        capture_output=True,
    )


class TargetCacheTests(unittest.TestCase):
    def test_dev_cargo_locks_and_maintains_explicit_target_dir(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fake_bin = root / "bin"
            fake_bin.mkdir()
            fake_cargo = fake_bin / "cargo"
            fake_cargo.write_text("#!/bin/sh\nexit 0\n")
            fake_cargo.chmod(0o755)
            target = root / "custom-target"
            env = os.environ.copy()
            env.update(
                {
                    "PATH": f"{fake_bin}{os.pathsep}{env['PATH']}",
                    "JCODE_PARALLEL_FRONTEND": "off",
                    "JCODE_SELFDEV_LOW_MEMORY": "off",
                    "JCODE_BUILD_JOBS": "1",
                    "JCODE_TARGET_CACHE_INTERVAL_HOURS": "0",
                }
            )

            subprocess.run(
                [
                    str(DEV_CARGO),
                    "metadata",
                    "--target-dir",
                    os.path.relpath(target, DEV_CARGO.parent.parent),
                ],
                cwd=DEV_CARGO.parent.parent,
                env=env,
                check=True,
                text=True,
                capture_output=True,
            )

            self.assertTrue((target / ".jcode-cache-maintenance-at").is_file())
            self.assertTrue(
                (root / ".jcode-target-cache-custom-target.lock").is_file()
            )

    def test_dev_cargo_resolves_relative_cargo_target_dir_env(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fake_bin = root / "bin"
            fake_bin.mkdir()
            fake_cargo = fake_bin / "cargo"
            fake_cargo.write_text("#!/bin/sh\nexit 0\n")
            fake_cargo.chmod(0o755)
            target = root / "environment-target"
            env = os.environ.copy()
            env.update(
                {
                    "PATH": f"{fake_bin}{os.pathsep}{env['PATH']}",
                    "CARGO_TARGET_DIR": os.path.relpath(
                        target, DEV_CARGO.parent.parent
                    ),
                    "JCODE_PARALLEL_FRONTEND": "off",
                    "JCODE_SELFDEV_LOW_MEMORY": "off",
                    "JCODE_BUILD_JOBS": "1",
                    "JCODE_TARGET_CACHE_INTERVAL_HOURS": "0",
                }
            )

            subprocess.run(
                [str(DEV_CARGO), "metadata"],
                cwd=DEV_CARGO.parent.parent,
                env=env,
                check=True,
                text=True,
                capture_output=True,
            )

            self.assertTrue((target / ".jcode-cache-maintenance-at").is_file())
            self.assertTrue(
                (root / ".jcode-target-cache-environment-target.lock").is_file()
            )

    def test_dev_cargo_uses_cargo_metadata_target_dir(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fake_bin = root / "bin"
            fake_bin.mkdir()
            target = root / "configured-target"
            fake_cargo = fake_bin / "cargo"
            fake_cargo.write_text(
                "#!/bin/sh\n"
                f"printf '%s\\n' '{{\"target_directory\":\"{target}\"}}'\n"
            )
            fake_cargo.chmod(0o755)
            env = os.environ.copy()
            env.pop("CARGO_TARGET_DIR", None)
            env.update(
                {
                    "PATH": f"{fake_bin}{os.pathsep}{env['PATH']}",
                    "JCODE_PARALLEL_FRONTEND": "off",
                    "JCODE_SELFDEV_LOW_MEMORY": "off",
                    "JCODE_BUILD_JOBS": "1",
                    "JCODE_TARGET_CACHE_INTERVAL_HOURS": "0",
                }
            )

            subprocess.run(
                [str(DEV_CARGO), "metadata"],
                cwd=DEV_CARGO.parent.parent,
                env=env,
                check=True,
                text=True,
                capture_output=True,
            )

            self.assertTrue((target / ".jcode-cache-maintenance-at").is_file())
            self.assertTrue(
                (root / ".jcode-target-cache-configured-target.lock").is_file()
            )

    def test_prune_removes_stale_and_surplus_generations_but_keeps_warm_set(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            now = time.time()
            incremental = root / "target" / "selfdev" / "incremental"
            old = incremental / "jcode-old"
            middle = incremental / "jcode-middle"
            newest = incremental / "jcode-newest"
            for index, path in enumerate((old, middle, newest)):
                write_sized(path / "artifact", 2048, now - (3 - index) * 3600)
                os.utime(path, (now - (3 - index) * 3600,) * 2)

            deps = root / "target" / "selfdev" / "deps"
            old_dep = deps / "demo-1111111111111111.rlib"
            middle_dep = deps / "demo-2222222222222222.rlib"
            newest_dep = deps / "demo-3333333333333333.rlib"
            write_sized(old_dep, 2048, now - 3 * 3600)
            write_sized(middle_dep, 2048, now - 2 * 3600)
            write_sized(newest_dep, 2048, now - 3600)

            build_cache = root / "target" / "selfdev" / "build" / "demo"
            old_build = build_cache / "1111111111111111"
            middle_build = build_cache / "2222222222222222"
            newest_build = build_cache / "3333333333333333"
            for index, path in enumerate((old_build, middle_build, newest_build)):
                write_sized(path / "artifact", 2048, now - (3 - index) * 3600)
                os.utime(path, (now - (3 - index) * 3600,) * 2)

            result = run_prune(root)

            self.assertIn("surplus", result.stderr)
            self.assertFalse(old.exists())
            self.assertTrue(middle.exists())
            self.assertTrue(newest.exists())
            self.assertFalse(old_dep.exists())
            self.assertTrue(middle_dep.exists())
            self.assertTrue(newest_dep.exists())
            self.assertFalse(old_build.exists())
            self.assertTrue(middle_build.exists())
            self.assertTrue(newest_build.exists())

    def test_shared_build_lock_prevents_concurrent_prune(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            stale = root / "target" / "selfdev" / "incremental" / "demo-old"
            write_sized(stale / "artifact", 4096, time.time() - 3 * 86400)
            os.utime(stale, (time.time() - 3 * 86400,) * 2)
            ready = root / "ready"
            holder = subprocess.Popen(
                [
                    sys.executable,
                    str(SCRIPT),
                    "run",
                    "--lock",
                    str(root / "target.lock"),
                    "--",
                    sys.executable,
                    "-c",
                    f"from pathlib import Path; import time; Path({str(ready)!r}).touch(); time.sleep(1)",
                ]
            )
            try:
                deadline = time.time() + 5
                while not ready.exists() and time.time() < deadline:
                    time.sleep(0.01)
                self.assertTrue(ready.exists(), "lock holder did not start")
                run_prune(root)
                self.assertTrue(stale.exists())
            finally:
                holder.wait(timeout=5)

            run_prune(root)
            self.assertFalse(stale.exists())

    def test_lock_survives_target_directory_recreation(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = root / "target"
            target.mkdir()
            ready = root / "ready"
            holder = subprocess.Popen(
                [
                    sys.executable,
                    str(SCRIPT),
                    "run",
                    "--lock",
                    str(root / "target.lock"),
                    "--",
                    sys.executable,
                    "-c",
                    f"from pathlib import Path; import time; Path({str(ready)!r}).touch(); time.sleep(1)",
                ]
            )
            try:
                deadline = time.time() + 5
                while not ready.exists() and time.time() < deadline:
                    time.sleep(0.01)
                self.assertTrue(ready.exists(), "lock holder did not start")
                shutil.rmtree(target)
                stale = target / "selfdev" / "incremental" / "demo-old"
                write_sized(stale / "artifact", 4096, time.time() - 3 * 86400)
                os.utime(stale, (time.time() - 3 * 86400,) * 2)
                run_prune(root)
                self.assertTrue(stale.exists())
            finally:
                holder.wait(timeout=5)

    def test_recent_maintenance_stamp_skips_rescan(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = root / "target"
            stale = target / "selfdev" / "incremental" / "demo-old"
            write_sized(stale / "artifact", 4096, time.time() - 3 * 86400)
            os.utime(stale, (time.time() - 3 * 86400,) * 2)
            target.mkdir(parents=True, exist_ok=True)
            (target / ".jcode-cache-maintenance-at").write_text("recent\n")

            run_prune(root, interval_hours=24)

            self.assertTrue(stale.exists())


if __name__ == "__main__":
    unittest.main()
