"""Evaluate lexical and hybrid retrieval against an isolated, labelled desktop corpus."""
import argparse
import json
import math
import os
from pathlib import Path
import socket
import statistics
import struct
import subprocess
import tempfile
import time
import zlib

DOCUMENTS = {
    "amber.txt": "The medical doctor examined the patient in the hospital clinic.",
    "oak.txt": "The mechanic repaired a broken gearbox in the vehicle workshop.",
    "cedar.txt": "Bakers knead wheat dough and bake fresh loaves in the oven.",
    "birch.txt": "The orchestra performed a symphony for violin and piano.",
    "fern.txt": "The university librarian catalogued the books and manuscripts.",
    "lake.txt": "The train leaves the railway station on the morning timetable.",
    "invoice-QX17.txt": "Invoice payment deadline and the agreed bank transfer amount.",
    "маршрут.txt": "Расписание поезда и время отправления с вокзала.",
    "договор-RU42.txt": "Условия поставки и срок оплаты согласованы.",
    "合同-ZH42.txt": "合同约定的交货日期和付款期限。",
    "车票.txt": "旅客乘坐火车，需要提前购买车票。",
}

# Split by query rather than collecting user search history. A document can have
# several equally valid answers in the same corpus.
CASES = [
    ("dev", "text", "physician", ["amber.txt"]),
    ("dev", "text", "car transmission", ["oak.txt"]),
    ("dev", "text", "pianist", ["birch.txt"]),
    ("dev", "visual", "orange square", ["tile.png"]),
    ("dev", "exact", "QX17", ["invoice-QX17.txt"]),
    ("dev", "multilingual", "поезд", ["маршрут.txt"]),
    ("dev", "multilingual", "交货", ["合同-ZH42.txt"]),
    ("holdout", "text", "healthcare professional", ["amber.txt"]),
    ("holdout", "text", "automobile technician", ["oak.txt"]),
    ("holdout", "text", "classical concert", ["birch.txt"]),
    ("holdout", "text", "making bread", ["cedar.txt"]),
    ("holdout", "text", "book collection", ["fern.txt"]),
    ("holdout", "text", "rail departure", ["lake.txt", "маршрут.txt", "车票.txt"]),
    ("holdout", "visual", "solid blue rectangle", ["panel.png"]),
    ("holdout", "visual", "a picture of an orange block", ["tile.png"]),
    ("holdout", "exact", "invoice-QX17", ["invoice-QX17.txt"]),
    ("holdout", "exact", "договор-RU42", ["договор-RU42.txt"]),
    ("holdout", "exact", "合同-ZH42", ["合同-ZH42.txt"]),
    ("holdout", "multilingual", "срок оплаты", ["договор-RU42.txt"]),
    ("holdout", "multilingual", "火车票", ["车票.txt"]),
]


def png(width, height, rgb):
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    pixels = (b"\0" + bytes(rgb) * width) * height
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(pixels)) + chunk(b"IEND", b""))


def read_exact(stream, size):
    data = stream.read(size)
    if len(data) != size:
        raise RuntimeError("incomplete daemon frame")
    return data


def request(runtime, payload):
    started = time.monotonic()
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(45)
        sock.connect(str(runtime / "lixun.sock"))
        data = json.dumps(payload).encode()
        sock.sendall(struct.pack(">IH", len(data) + 2, 4) + data)
        with sock.makefile("rb") as stream:
            while True:
                size, version = struct.unpack(">IH", read_exact(stream, 6))
                assert version == 4
                result = json.loads(read_exact(stream, size - 2))
                if "Error" in result:
                    raise RuntimeError(result["Error"])
                yield result, (time.monotonic() - started) * 1000
                if result.get("SearchChunk", {}).get("phase") != "Initial":
                    return


def search(runtime, query):
    pages = list(request(runtime, {"Search": {"q": query, "limit": 10, "explain": False, "epoch": 1}}))
    return [[hit["title"] for hit in page["SearchChunk"]["hits"]] for page, _ in pages], pages[0][1]


def evaluate(args, root):
    corpus = root / "corpus"
    runtime = root / "run"
    config = root / "config/lixun"
    for directory in (corpus, runtime, config, root / "home"):
        directory.mkdir(mode=0o700, parents=True)
    for name, content in DOCUMENTS.items():
        (corpus / name).write_text(content)
    (corpus / "tile.png").write_bytes(png(160, 160, (255, 143, 0)))
    (corpus / "panel.png").write_bytes(png(240, 140, (0, 55, 255)))
    (config / "config.toml").write_text(f'[core]\nroots = [{json.dumps(str(corpus))}]\n[semantic]\nenabled = true\n[ocr]\nenabled = false\n')
    env = dict(os.environ, DBUS_SESSION_BUS_ADDRESS=f"unix:path={runtime}/no-session-bus", HOME=str(root / "home"), XDG_CONFIG_HOME=str(config.parent),
               XDG_RUNTIME_DIR=str(runtime), XDG_STATE_HOME=str(root / "state"),
               XDG_CACHE_HOME=str(root / "cache"), XDG_DATA_HOME=str(root / "data"),
               LIXUN_SEMANTIC_WORKER=str(args.worker.resolve()), LIXUN_SEMANTIC_DATA_DIR=str(root / "vectors"),
               LIXUN_SEMANTIC_CACHE_DIR=str(args.cache.resolve()), HF_HUB_OFFLINE="1", RUST_LOG="warn")
    with (root / "daemon.log").open("w") as log:
        daemon = subprocess.Popen([args.daemon.resolve()], env=env, stdout=log, stderr=log)
        try:
            wait_for = time.monotonic() + 150
            while time.monotonic() < wait_for:
                if daemon.poll() is not None:
                    raise RuntimeError((root / "daemon.log").read_text()[-1400:])
                try:
                    status = next(request(runtime, "Status"))[0]["Status"]
                    if status["semantic"]["state"] == "Ready" and status["indexed_docs"] >= len(DOCUMENTS) + 2:
                        break
                except (KeyError, OSError):
                    pass
                time.sleep(.25)
            else:
                raise RuntimeError("index or worker did not become ready")
            backfill = subprocess.check_output([args.cli.resolve(), "semantic", "backfill"], env=env, timeout=120)
            assert json.loads(backfill)["total"] >= len(DOCUMENTS) + 2
            # The backfill acknowledges submission. Wait for real ANN-only results
            # from both channels before measuring latency or relevance.
            until = time.monotonic() + 120
            while time.monotonic() < until:
                pages, _ = search(runtime, "physician")
                image_pages, _ = search(runtime, "orange square")
                if len(pages) == len(image_pages) == 2 and "amber.txt" in pages[-1] and "tile.png" in image_pages[-1]:
                    break
                time.sleep(.5)
            else:
                raise RuntimeError("both retrieval channels did not populate")
            rows = []
            for split, group, query, relevant in CASES:
                pages, first_ms = search(runtime, query)
                for mode, hits in (("lexical", pages[0]), ("hybrid", pages[-1])):
                    ranks = [index + 1 for index, name in enumerate(hits) if name in relevant]
                    dcg = sum(1 / math.log2(1 + rank) for rank in ranks)
                    ideal = sum(1 / math.log2(2 + i) for i in range(len(relevant)))
                    rows.append(dict(split=split, group=group, query=query, mode=mode,
                                     ndcg10=dcg / ideal, recall5=sum(r <= 5 for r in ranks) / len(relevant),
                                     top1=int(1 in ranks), first_ms=first_ms, hits=hits))
            return rows
        finally:
            daemon.terminate()
            try:
                daemon.wait(10)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for arg in ("daemon", "cli", "worker", "cache", "output"):
        parser.add_argument("--" + arg, type=Path, required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="lixun-quality-") as folder:
        rows = evaluate(args, Path(folder))
    args.output.write_text(json.dumps(rows, ensure_ascii=False, indent=2) + "\n")
    for mode in ("lexical", "hybrid"):
        for group in ("text", "visual", "exact", "multilingual"):
            selected = [r for r in rows if r["split"] == "holdout" and r["mode"] == mode and r["group"] == group]
            metrics = {k: round(statistics.mean(r[k] for r in selected), 3) for k in ("ndcg10", "recall5", "top1")}
            print(mode, group, metrics)
    times = sorted(r["first_ms"] for r in rows if r["mode"] == "hybrid")
    print("First page p95 (ms)", round(times[math.ceil(.95 * len(times)) - 1], 2))
