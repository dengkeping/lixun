"""Release daemon regression tests on an isolated profile and socket."""
import argparse
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import tempfile
import time


def until(probe, timeout=65):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if result := probe():
                return result
        except (ConnectionError, FileNotFoundError):
            pass
        time.sleep(.12)
    raise AssertionError(f"timed out waiting for {probe}")


def exercise(binaries, socket_tests, profile):
    root = profile / "corpus"
    cfg = profile / "config/lixun/config.toml"
    state = profile / "state/lixun"
    runtime = profile / "run"
    for directory in (root, cfg.parent, state, runtime, profile / "home", profile / "empty"):
        directory.mkdir(mode=0o700, parents=True)
    env = dict(os.environ, HOME=str(profile / "home"), XDG_CONFIG_HOME=str(cfg.parent.parent),
               XDG_DATA_HOME=str(profile / "data"), XDG_DATA_DIRS=str(profile / "empty"),
               XDG_CONFIG_DIRS=str(profile / "empty"), XDG_STATE_HOME=str(state.parent),
               XDG_CACHE_HOME=str(profile / "cache"), XDG_RUNTIME_DIR=str(runtime),
               DBUS_SESSION_BUS_ADDRESS=f"unix:path={runtime}/no-session-bus", RUST_LOG="lixun=debug")
    for name in ("DISPLAY", "WAYLAND_DISPLAY", "SWAYSOCK"):
        env.pop(name, None)
    cfg.write_text("[core\n")
    valid = f'[core]\nroots = [{json.dumps(str(root))}]\nexclude_regex = ["/blocked$"]\n[semantic]\nenabled=false\n[ocr]\nenabled=false\n[shell]\nstrict_mode=true\n'
    docs = [root / f"reference-{i}.txt" for i in range(3)]
    for doc in docs:
        doc.write_text("searchable reference")
    apps = profile / "data/applications"
    apps.mkdir(parents=True)
    (apps / "wirefixture.desktop").write_text("[Desktop Entry]\nType=Application\nName=Wirefixture\nExec=true\n")
    denied = root / "blocked"
    denied.mkdir()
    secret = denied / "excludedfixture.txt"
    secret.write_text("excludedfixture")
    outside = profile / "outoffixture.txt"
    outside.write_text("outoffixture")
    (root / "alias").symlink_to(outside)
    marker = "privatefixture9042"
    legacy = "historyfixture531"
    hidden = "shell:echo hiddenfixture"
    allowed = "reference"
    allowed_id = f"fs:{docs[0]}"

    def cli(*args):
        return subprocess.run([binaries / "lixun-cli", *args], env=env, capture_output=True, text=True, timeout=18)

    def frames(message):
        data = json.dumps(message).encode()
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(12)
            client.connect(str(runtime / "lixun.sock"))
            client.sendall(struct.pack(">IH", len(data) + 2, 4) + data)
            with client.makefile("rb") as stream:
                while True:
                    size, version = struct.unpack(">IH", stream.read(6))
                    assert version == 4 and 2 <= size <= 64 * 1024**2
                    response = json.loads(stream.read(size - 2))
                    yield response
                    if not isinstance(response, dict) or response.get("SearchChunk", {}).get("phase") != "Initial":
                        return

    def rpc(message):
        return list(frames(message))[-1]

    def search(query, limit=10, category=None):
        chunk = rpc({"Search": dict(q=query, limit=limit, epoch=1, explain=True, category=category)})["SearchChunk"]
        ids = [hit["id"] for hit in chunk["hits"]]
        assert chunk["top_hit"] is None or chunk["top_hit"] in ids
        return ids

    def count():
        return rpc("Status").get("Status", {}).get("indexed_docs", -1)

    with (profile / "events.log").open("w") as log:
        def start():
            return subprocess.Popen([binaries / "lixund"], env=env, stdout=log, stderr=log)

        daemon = start()
        try:
            until(lambda: "Error" in rpc("Status"))
            status = cli("status")
            assert status.returncode != 0 and "invalid TOML" in status.stderr
            assert not (state / "index").exists()
            cfg.write_text(valid)
            until(lambda: count() == 4)
            capabilities = rpc("Capabilities")["Capabilities"]
            assert capabilities["versions"] == [4, 5, 6] and capabilities["record_selection"]
            for path in (secret, denied, outside, root / "alias"):
                assert cli("check-exclude", str(path)).stdout.startswith("EXCLUDED by ")
            rpc({"Reindex": {"paths": list(map(str, (secret, denied, outside, root / "alias")))}})
            assert not search("excludedfixture") and not search("outoffixture")

            if socket_tests:
                subprocess.run([socket_tests, "--ignored", "--test-threads=1"], env=env, check=True, timeout=60)
            assert rpc({"RecordSelection": dict(doc_id=allowed_id, query=allowed)}) == "Ok"
            for verb, args in [
                ("RecordSelection", dict(doc_id=f"shell:{marker}", query=f"> {marker}")),
                ("RecordQueryClick", dict(doc_id=allowed_id, query=f"> {marker}")),
                ("RecordClick", dict(doc_id=f"shell:{marker}")), ("RecordQuery", dict(q=f"> {marker}")),
            ]:
                assert rpc({verb: args}) == "Ok"
            search(f"> echo {marker} | sort")
            assert hidden in search("> echo hiddenfixture")
            # A failed persist must leave visibility retriable.
            (state / "blocklist.json.tmp").mkdir()
            assert "Error" in rpc({"SetDocHidden": dict(doc_id=hidden, hidden=True)})
            assert hidden in search("> echo hiddenfixture")
            (state / "blocklist.json.tmp").rmdir()
            assert rpc({"SetDocHidden": dict(doc_id=hidden, hidden=True)}) == "Ok"
            assert hidden not in search("> echo hiddenfixture")
            assert search("reference", 1, "File")
            daemon.terminate()
            assert daemon.wait(timeout=20) == 0
            for name in ("queries", "frecency", "query_latch"):
                data = (state / f"{name}.json").read_text()
                assert marker not in data and allowed in data
            assert marker not in (profile / "events.log").read_text()

            history = state / "queries.json"
            entries = json.loads(history.read_text())
            entries["entries"].append(f"> {legacy}")
            history.write_text(json.dumps(entries))
            clicks = state / "frecency.json"
            records = json.loads(clicks.read_text())
            records["records"][f"shell:{legacy}"] = records["records"][allowed_id]
            clicks.write_text(json.dumps(records))
            latch = state / "query_latch.json"
            links = json.loads(latch.read_text())
            links["by_query"][f"> {legacy}"] = {f"shell:{legacy}": {"count": 1, "last_ts": 1}}
            latch.write_text(json.dumps(links))
            cfg.write_text(valid + "enabled=false\n")
            daemon = start()
            until(lambda: count() == 4)
            expected = {"queries": 1, "clicks": 1, "associations": 1}
            assert rpc({"CleanPrivateHistory": {"apply": False}})["PrivateHistory"] == expected
            assert all(legacy in store.read_text() for store in (history, clicks, latch))
            applied = cli("clean-private-history", "--apply")
            assert applied.returncode == 0 and legacy not in applied.stdout
            assert all(legacy not in store.read_text() for store in (history, clicks, latch))
            assert rpc({"SearchHistory": {"limit": 20}})["Queries"] == [allowed]
            daemon.terminate()
            assert daemon.wait(timeout=20) == 0
            cfg.write_text(valid)
            daemon = start()
            until(lambda: count() == 4)
            assert hidden not in search("> echo hiddenfixture")
            rpc({"Reindex": {"paths": []}})
            until(lambda: search(allowed) and count() == 4)
            docs[-1].unlink()
            until(lambda: count() == 3)

            # Each imported directory exceeds the shared job queue capacity.
            for n in range(2):
                staged = profile / f"staged-{n}"
                staged.mkdir()
                for i in range(1030):
                    (staged / f"batch-{n}-{i}.txt").touch()
                staged.rename(root / f"batch-{n}")
            removed = root / "batch-0/batch-0-0.txt"
            removed.unlink()
            moved = root / "batch-1/movedfixture.txt"
            (root / "batch-1/batch-1-0.txt").rename(moved)
            until(lambda: count() == 2064 and f"fs:{moved}" in search("movedfixture"), timeout=120)
            assert f"fs:{removed}" not in search("batch-0-0.txt")
            sentinel = root / "afterimportfixture.txt"
            sentinel.touch()
            until(lambda: f"fs:{sentinel}" in search("afterimportfixture"))
        finally:
            if daemon.poll() is None:
                daemon.terminate()
                try:
                    daemon.wait(15)
                except subprocess.TimeoutExpired:
                    daemon.kill()
                    daemon.wait()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binaries", type=Path, required=True)
    parser.add_argument("--socket-tests", type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="lixun-daemon-test-") as scratch:
        exercise(args.binaries.resolve(), args.socket_tests and args.socket_tests.resolve(), Path(scratch))
    print("Daemon regression checks passed")
