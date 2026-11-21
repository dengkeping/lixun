"""Interrupt a schema upgrade during extraction, then verify the published generation."""

import argparse
import json
import os
import signal
import subprocess
import tempfile
import time
from pathlib import Path

from evaluate import request


def wait_for(probe):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        try:
            if probe():
                return
        except (ConnectionError, FileNotFoundError):
            pass
        time.sleep(0.05)
    raise AssertionError(f"timed out at {probe}")


def exercise(root, binaries, legacy):
    docs, runtime, config = root / "docs", root / "run", root / "config/lixun"
    for directory in (
        docs,
        runtime,
        config,
        root / "home",
        root / "empty",
        root / "bin",
    ):
        directory.mkdir(mode=0o700, parents=True)
    env = dict(
        os.environ,
        HOME=str(root / "home"),
        XDG_DATA_DIRS=str(root / "empty"),
        XDG_CONFIG_HOME=str(config.parent),
        XDG_DATA_HOME=str(root / "data"),
        XDG_RUNTIME_DIR=str(runtime),
        XDG_STATE_HOME=str(root / "state"),
        XDG_CACHE_HOME=str(root / "cache"),
        DBUS_SESSION_BUS_ADDRESS="unix:path=/dev/null",
        PATH=f"{root}/bin:{os.environ['PATH']}",
        FIXTURE_PROFILE=str(root),
    )
    (config / "config.toml").write_text(
        f'[core]\nroots=[{json.dumps(str(docs))}]\nexclude=["scope-denied"]\n[semantic]\nenabled=false\n[ocr]\nenabled=false\n'
    )
    (docs / "existing.txt").write_text("amber ledger")
    index = root / "state/lixun/index"
    with (root / "daemon.log").open("w") as log:

        def start(folder):
            return subprocess.Popen(
                [folder / "lixund"],
                env=env,
                stdout=log,
                stderr=log,
                start_new_session=True,
            )

        def stop(daemon, sig):
            if daemon.poll() is None:
                os.killpg(daemon.pid, sig)
                daemon.wait(15)

        def titles(query):
            pages = list(
                request(
                    runtime,
                    {"Search": {"q": query, "limit": 10, "explain": False, "epoch": 1}},
                )
            )
            return [h["title"] for h in pages[-1][0]["SearchChunk"]["hits"]]

        daemon = start(legacy or binaries)
        try:
            wait_for(lambda: "existing.txt" in titles("amber"))
            if legacy is None:
                wait_for(lambda: (index / "CURRENT").is_file())
            stop(daemon, signal.SIGTERM)
            pointer = index / "CURRENT"
            old_name = pointer.read_text() if pointer.exists() else None
            old = index / old_name if old_name else index
            (old / "index_version.txt").write_text("0")
            disconnected = root / "disconnected"
            docs.rename(disconnected)
            daemon = start(binaries)
            wait_for(lambda: "replacement missing" in (root / "daemon.log").read_text())
            assert (pointer.read_text() if pointer.exists() else None) == old_name
            assert titles("amber") == ["existing.txt"]
            stop(daemon, signal.SIGTERM)
            disconnected.rename(docs)
            (docs / "scope-denied.txt").write_text("private")
            (docs / "opaque.bin").write_bytes(bytes(90))
            (docs / "second.pdf").write_bytes(b"%PDF-1.4\n")
            fake = root / "bin/pdftotext"
            fake.write_text(
                '#!/bin/sh\ntouch "$FIXTURE_PROFILE/processing"\nwhile test -f "$FIXTURE_PROFILE/pause"; do sleep .05; done\nprintf "saffron ledger"\n'
            )
            fake.chmod(0o700)
            (root / "pause").touch()
            daemon = start(binaries)
            wait_for(lambda: (root / "processing").exists())
            assert titles("amber") == ["existing.txt"]
            assert titles("saffron") == []
            assert (pointer.read_text() if pointer.exists() else None) == old_name
            pending_name = (index / "PENDING").read_text()
            stop(daemon, signal.SIGKILL)
            assert (old / "meta.json").is_file()
            assert (pointer.read_text() if pointer.exists() else None) == old_name
            (root / "pause").unlink()
            daemon = start(binaries)
            wait_for(lambda: pointer.exists() and pointer.read_text() == pending_name)
            assert titles("saffron") == ["second.pdf"]
            assert titles("amber") == ["existing.txt"]
            assert "scope-denied.txt" not in titles("scope-denied")
            assert (old / "meta.json").is_file()
            preview = subprocess.check_output(
                [binaries / "lixun-cli", "preview-root", docs], env=env, text=True
            )
            for prefix, name in [
                ("content", "existing.txt"),
                ("content", "second.pdf"),
                ("name (no extractable text)", "opaque.bin"),
                ("excluded (substring scope-denied)", "scope-denied.txt"),
            ]:
                assert f"{prefix}\t{docs / name}" in preview, preview
        finally:
            stop(daemon, signal.SIGTERM)


if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--bin", type=Path, required=True)
    p.add_argument("--legacy-bin", type=Path)
    args = p.parse_args()
    with tempfile.TemporaryDirectory(prefix="lixun-upgrade-") as tmp:
        exercise(
            Path(tmp), args.bin.resolve(), args.legacy_bin and args.legacy_bin.resolve()
        )
    print(
        "Verified offline-source protection, interruption, old-reader availability, resumption, publication and scope preview."
    )
