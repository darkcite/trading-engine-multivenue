# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Gate G-S4 — retention.sh stage B, exercised as a script.

`retention.sh` is the only thing in this whole lane that DELETES capture data,
and it runs unattended at the 0000Z slot. So it gets tested the way it runs: as
zsh, on a seeded directory tree, with the archiver replaced by a stub whose exit
code we choose.

Two properties:

1. **A run dir survives every non-zero verify.** Exit 1 (not in the bucket) and
   exit 3 (subsystem disabled) must both keep the data and stop the sweep. The
   failure mode of this script must always be "keep", never "delete against an
   archive that isn't there".
2. **ARCHIVE_MODE=compress is byte-for-byte the old behaviour.** The new script
   is diffed against `git show HEAD:scripts/retention.sh` on the same seeded
   root — that is the S-LAW 2 proof for the restart lane, where a regression
   would be discovered by finding data missing.

Convention: full ``import x`` only. No ``from x import y``.
"""

import os
import pathlib
import shutil
import subprocess
import time
import typing

import pytest

REPO = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = REPO / "scripts" / "retention.sh"

pytestmark = pytest.mark.skipif(shutil.which("zsh") is None, reason="zsh not installed")


def seed_root(tmp_path: pathlib.Path, runs: int = 3) -> pathlib.Path:
    """Run dirs old enough to be past any PROTECT_DAYS the test sets."""
    root = tmp_path / "logs"
    root.mkdir(parents=True)
    # 2023-ish epochs: comfortably older than PROTECT_DAYS in every case.
    for i in range(runs):
        run = root / f"run-{1_690_000_000_000_000_000 + i * 10**12}"
        run.mkdir()
        (run / "pm-ticks.pmlr").write_bytes(b"PMLR" + b"\x00" * 60)
    return root


def fake_home(tmp_path: pathlib.Path, exit_code: int) -> pathlib.Path:
    """A HOME whose multivenue/venv/bin/python3 is a stub with a chosen exit."""
    home = tmp_path / "home"
    binary = home / "multivenue" / "venv" / "bin"
    binary.mkdir(parents=True)
    stub = binary / "python3"
    stub.write_text(
        "#!/bin/sh\n"
        '# stub archiver: records the verb + run id, then exits as instructed\n'
        'echo "stub $*" >> "$STUB_LOG"\n'
        f"exit {exit_code}\n",
        encoding="utf-8",
    )
    stub.chmod(0o755)
    (home / "multivenue" / "archive").mkdir(parents=True, exist_ok=True)
    return home


def run_script(
    script: pathlib.Path,
    root: pathlib.Path,
    conf: pathlib.Path,
    home: pathlib.Path,
    stub_log: pathlib.Path,
) -> subprocess.CompletedProcess[str]:
    env = dict(os.environ)
    env["HOME"] = str(home)
    env["STUB_LOG"] = str(stub_log)
    return subprocess.run(
        ["zsh", str(script), "--root", str(root), "--conf", str(conf)],
        capture_output=True,
        text=True,
        check=False,
        env=env,
    )


def write_conf(path: pathlib.Path, **values: typing.Any) -> pathlib.Path:
    path.write_text(
        "\n".join(f'{k}="{v}"' if isinstance(v, str) else f"{k}={v}" for k, v in values.items()),
        encoding="utf-8",
    )
    return path


# --------------------------------------------------------------------------
# stage B
# --------------------------------------------------------------------------


def test_verified_run_is_deleted(tmp_path: pathlib.Path) -> None:
    root = seed_root(tmp_path)
    home = fake_home(tmp_path, exit_code=0)
    stub_log = tmp_path / "stub.log"
    conf = write_conf(
        tmp_path / "retention.conf",
        MIN_FREE_GIB=999999,  # force the pressure branch
        TARGET_FREE_GIB=999999,  # never satisfied -> walk every candidate
        PROTECT_DAYS=0,
        ARCHIVE_MODE="s3",
    )
    before = sorted(p.name for p in root.iterdir())
    done = run_script(SCRIPT, root, conf, home, stub_log)
    assert done.returncode == 0
    after = sorted(p.name for p in root.iterdir())
    # oldest-first, newest never a candidate
    assert after == [before[-1]]
    assert "verify" in stub_log.read_text(encoding="utf-8")


@pytest.mark.parametrize("code", [1, 3])
def test_unverified_run_survives_and_the_sweep_stops(
    code: int, tmp_path: pathlib.Path
) -> None:
    """Exit 1 = not in the bucket. Exit 3 = subsystem disabled. Both must keep."""
    root = seed_root(tmp_path)
    home = fake_home(tmp_path, exit_code=code)
    stub_log = tmp_path / "stub.log"
    conf = write_conf(
        tmp_path / "retention.conf",
        MIN_FREE_GIB=999999,
        TARGET_FREE_GIB=999999,
        PROTECT_DAYS=0,
        ARCHIVE_MODE="s3",
    )
    before = sorted(p.name for p in root.iterdir())
    done = run_script(SCRIPT, root, conf, home, stub_log)
    assert done.returncode == 0
    assert sorted(p.name for p in root.iterdir()) == before, "no run may be deleted"
    assert "NOT verified" in done.stderr
    # break, not continue: exactly one candidate was tried.
    assert stub_log.read_text(encoding="utf-8").count("verify") == 1


def test_missing_archiver_falls_back_to_compress_and_never_deletes_unverified(
    tmp_path: pathlib.Path,
) -> None:
    """ARCHIVE_MODE=s3 with nothing installed must degrade to the old, safe
    behaviour — not to deleting."""
    root = seed_root(tmp_path)
    home = tmp_path / "empty-home"
    (home / "multivenue").mkdir(parents=True)
    stub_log = tmp_path / "stub.log"
    conf = write_conf(
        tmp_path / "retention.conf",
        MIN_FREE_GIB=999999,
        TARGET_FREE_GIB=999999,
        PROTECT_DAYS=0,
        ARCHIVE_MODE="s3",
        ARCHIVE_DIR=str(tmp_path / "archive"),
    )
    done = run_script(SCRIPT, root, conf, home, stub_log)
    assert done.returncode == 0
    assert "not installed" in done.stderr
    assert "using compress" in done.stderr
    assert sorted((tmp_path / "archive").glob("*.tar.gz"))


# --------------------------------------------------------------------------
# the S-LAW 2 proof: compress mode is unchanged
# --------------------------------------------------------------------------


def old_script(tmp_path: pathlib.Path) -> pathlib.Path | None:
    done = subprocess.run(
        ["git", "--no-optional-locks", "show", "HEAD:scripts/retention.sh"],
        capture_output=True,
        text=True,
        check=False,
        cwd=REPO,
    )
    if done.returncode != 0 or not done.stdout:
        return None
    path = tmp_path / "old-retention.sh"
    path.write_text(done.stdout, encoding="utf-8")
    return path


def test_compress_mode_is_identical_to_the_previous_script(
    tmp_path: pathlib.Path,
) -> None:
    old = old_script(tmp_path)
    if old is None:
        pytest.skip("HEAD:scripts/retention.sh unavailable")

    def exercise(script: pathlib.Path, tag: str) -> tuple[str, list[str], list[str]]:
        base = tmp_path / tag
        base.mkdir()
        root = seed_root(base)
        home = tmp_path / f"home-{tag}"
        (home / "multivenue").mkdir(parents=True)
        conf = write_conf(
            base / "retention.conf",
            MIN_FREE_GIB=999999,
            TARGET_FREE_GIB=999999,
            PROTECT_DAYS=0,
            ARCHIVE_MODE="compress",
            ARCHIVE_DIR=str(base / "archive"),
        )
        done = run_script(script, root, conf, home, base / "stub.log")
        assert done.returncode == 0
        return (
            done.stderr.replace(str(base), "<BASE>"),
            sorted(p.name for p in root.iterdir()),
            sorted(p.name for p in (base / "archive").iterdir()),
        )

    new_stderr, new_runs, new_archive = exercise(SCRIPT, "new")
    old_stderr, old_runs, old_archive = exercise(old, "old")
    assert new_runs == old_runs
    assert new_archive == old_archive
    assert new_stderr == old_stderr


def test_no_pressure_still_exits_early_and_touches_nothing(
    tmp_path: pathlib.Path,
) -> None:
    root = seed_root(tmp_path)
    home = fake_home(tmp_path, exit_code=0)
    stub_log = tmp_path / "stub.log"
    conf = write_conf(
        tmp_path / "retention.conf",
        MIN_FREE_GIB=0,  # never any pressure
        TARGET_FREE_GIB=0,
        PROTECT_DAYS=0,
        ARCHIVE_MODE="s3",
    )
    before = sorted(p.name for p in root.iterdir())
    done = run_script(SCRIPT, root, conf, home, stub_log)
    assert done.returncode == 0
    assert sorted(p.name for p in root.iterdir()) == before
    assert not stub_log.exists(), "keep-all must not even ask the bucket"


def test_protect_days_still_stops_the_sweep(tmp_path: pathlib.Path) -> None:
    """The protection that predates this lane must survive it."""
    root = tmp_path / "logs"
    root.mkdir()
    now_ns = int(time.time() * 1e9)
    for i in range(3):
        (root / f"run-{now_ns - i * 10**12}").mkdir()
    home = fake_home(tmp_path, exit_code=0)
    stub_log = tmp_path / "stub.log"
    conf = write_conf(
        tmp_path / "retention.conf",
        MIN_FREE_GIB=999999,
        TARGET_FREE_GIB=999999,
        PROTECT_DAYS=5,
        ARCHIVE_MODE="s3",
    )
    before = sorted(p.name for p in root.iterdir())
    done = run_script(SCRIPT, root, conf, home, stub_log)
    assert done.returncode == 0
    assert sorted(p.name for p in root.iterdir()) == before
    assert "protect" in done.stderr
    assert not stub_log.exists()


def test_scripts_keep_their_exec_bit(tmp_path: pathlib.Path) -> None:
    """The 2026-08-27 incident: a repo-wide rewrite created new inodes at the
    umask and silently stripped the exec bit off five launchd scripts.
    `git diff --numstat` does not show mode changes; this does."""
    for name in ("retention.sh", "archive-cycle.sh", "install-launchd.sh"):
        path = REPO / "scripts" / name
        assert path.exists(), name
        assert os.access(path, os.X_OK), f"{name} lost its exec bit"
