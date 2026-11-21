"""Release evaluation. Supply the original Oxford-IIIT Pet images.tar.gz and a model cache."""

import argparse
import hashlib
import json
import math
import os
import signal
import sqlite3
import statistics
import subprocess
import tarfile
import tempfile
import time
from pathlib import Path

from evaluate import request
from PIL import Image, ImageDraw
from reportlab.pdfgen import canvas

BREEDS = (
    "beagle",
    "Abyssinian",
    "boxer",
    "Bengal",
    "leonberger",
    "Birman",
    "pug",
    "Persian",
    "samoyed",
    "Egyptian_Mau",
)
NAMES = [f"IMG_{1042 + i}.jpg" for i in range(len(BREEDS))]
DOGS, CATS = NAMES[::2], NAMES[1::2]
CASES = [
    ("dev", "visual", "en", "photos of cats", CATS),
    ("dev", "visual", "ru", "фото кошек", CATS),
    ("dev", "visual", "zh", "猫的照片", CATS),
    ("dev", "ambiguous", "en", "cats", CATS + ["care-A.txt"]),
    ("dev", "text", "en", "article about cat care", ["care-A.txt"]),
    ("dev", "cross-language", "ru", "расписание поездов", ["notes-T.txt"]),
    ("holdout", "visual", "en", "photos of dogs", DOGS),
    ("holdout", "visual", "ru", "фото собак", DOGS),
    ("holdout", "visual", "zh", "狗的照片", DOGS),
    ("holdout", "text", "en", "article about dog care", ["care-B.txt"]),
    ("holdout", "text", "ru", "статья об уходе за собакой", ["care-B.txt"]),
    ("holdout", "text", "zh", "关于照顾狗的文章", ["care-B.txt"]),
    ("holdout", "cross-language", "zh", "医院的医师", ["notes-H.txt"]),
    ("holdout", "exact", "en", NAMES[0], [NAMES[0]]),
    ("holdout", "filtered", "en", 'title:"care" -body:feline', ["care-B.txt"]),
    ("holdout", "filtered", "en", '"departure timetable"', ["notes-T.txt"]),
    ("holdout", "category", "en", "photographs of dogs", []),
    ("holdout", "unanswerable-visual", "en", "photos of elephants", []),
    ("holdout", "absent", "en", 'title:"nonexistentquartz"', []),
]


def populate(corpus, archive, skew):
    with archive.open("rb") as stream:
        if (
            hashlib.file_digest(stream, "sha256").hexdigest()
            != "67195c5e1c01f1ab5f9b6a5d22b8c27a580d896ece458917e61d459337fa318d"
        ):
            raise ValueError("unexpected Oxford-IIIT Pet archive checksum")
    with tarfile.open(archive) as pets:
        for breed, name in zip(BREEDS, NAMES):
            source = pets.extractfile(f"images/{breed}_1.jpg")
            (corpus / name).write_bytes(source.read())
    docs = {
        "care-A.txt": "Article about cat care. Feline health, feeding and grooming. Статья об уходе за кошкой. 关于照顾猫的文章。",
        "care-B.txt": "Article about dog care. Canine health, exercise and feeding. Статья об уходе за собакой. 关于照顾狗的文章。",
        "notes-T.txt": "The railway station posts the train departure timetable.",
        "notes-H.txt": "The physician examined the patient in the hospital clinic.",
    }
    for i in range(skew):
        docs[f"minutes-{i:03}.txt"] = (
            f"Meeting {i}. The editors discussed publication rights for photos of dogs and photos of cats."
        )
    for name, body in docs.items():
        (corpus / name).write_text(body)
    for idx, label in enumerate(("dogs", "cats")):
        img = Image.new("RGB", (640, 400), "white")
        draw = ImageDraw.Draw(img)
        draw.rectangle((0, 0, 640, 45), fill="navy")
        draw.text((50, 110), label, fill="black", font_size=72)
        img.save(corpus / f"screen-{idx}.png")
    img.convert("L").save(corpus / "scan.pdf")
    pdf = canvas.Canvas(str(corpus / "memo.pdf"))
    pdf.drawString(72, 720, "Publication schedule for the next quarterly report.")
    pdf.save()
    return set(docs) | set(NAMES) | {"screen-0.png", "screen-1.png", "memo.pdf"}


def evaluate(args, root):
    corpus, runtime, config = root / "corpus", root / "run", root / "config/lixun"
    for directory in (corpus, runtime, config, root / "home", root / "empty"):
        directory.mkdir(mode=0o700, parents=True)
    embedded = populate(corpus, args.pets, args.skew)
    model_config = f'image_model = "{args.image_model}"\n' if args.image_model else ""
    (config / "config.toml").write_text(
        f"[core]\nroots = [{json.dumps(str(corpus))}]\n"
        f'[semantic]\nenabled = true\ntext_model = "{args.text_model}"\n{model_config}'
        "[ocr]\nenabled = false\n"
    )
    env = dict(
        os.environ,
        HOME=str(root / "home"),
        XDG_CONFIG_HOME=str(config.parent),
        XDG_RUNTIME_DIR=str(runtime),
        XDG_STATE_HOME=str(root / "state"),
        XDG_CACHE_HOME=str(root / "cache"),
        XDG_DATA_HOME=str(root / "data"),
        XDG_DATA_DIRS=str(root / "empty"),
        DBUS_SESSION_BUS_ADDRESS=f"unix:path={runtime}/no-bus",
        LIXUN_SEMANTIC_CACHE_DIR=str(args.cache.resolve()),
        LIXUN_SEMANTIC_DATA_DIR=str(root / "vectors"),
        LIXUN_SEMANTIC_WORKER=str(
            Path(__file__).with_name("worker_trace.py").resolve()
        ),
        QUALITY_WORKER=str((args.bin / "lixun-semantic-worker").resolve()),
        QUALITY_TRACE=str(root / "trace"),
        LIXUN_LOG="info",
        RUST_LOG="lixun=info,semantic_worker=info,lixun_fusion::ranks=trace",
    )
    if not args.allow_download:
        env["HF_HUB_OFFLINE"] = "1"
    started = time.monotonic()
    log = root / "daemon.log"
    with log.open("w") as stream:
        daemon = subprocess.Popen(
            [args.bin / "lixund"],
            env=env,
            stdout=stream,
            stderr=stream,
            start_new_session=True,
        )
    try:

        def wait(check):
            until = time.monotonic() + (1200 if args.allow_download else 240)
            while time.monotonic() < until and daemon.poll() is None:
                if check():
                    return
                time.sleep(0.25)
            raise RuntimeError(
                "profile did not become ready: " + log.read_text()[-3000:]
            )

        wait(
            lambda: (
                (runtime / "lixun.sock").exists()
                and "query router ready" in log.read_text()
                and next(request(runtime, "Status"))[0]["Status"]["indexed_docs"]
                >= len(embedded)
            )
        )
        subprocess.run(
            [args.bin / "lixun-cli", "semantic", "backfill"],
            env=env,
            check=True,
            capture_output=True,
        )
        (journal,) = (root / "vectors").rglob("semantic-backfill.sqlite")

        def indexed():
            with sqlite3.connect(journal) as db:
                return {
                    Path(row[0]).name
                    for row in db.execute("select doc_id from progress")
                }

        wait(lambda: embedded <= indexed())
        build_seconds = time.monotonic() - started
        rows = []
        for split, group, lang, query, labels in CASES:
            if args.split != "all" and args.split != split:
                continue
            search = {"q": query, "limit": 10, "explain": False, "epoch": 1}
            if group == "category":
                search["category"] = "Mail"
            offset = log.stat().st_size
            samples = [
                list(request(runtime, {"Search": search}))
                for _ in range(args.repetitions)
            ]
            pages = samples[0]
            for sample in samples:
                assert sample[0][0]["SearchChunk"]["phase"] == "Initial"
                assert sample[-1][0]["SearchChunk"]["phase"] == "Final"
            with log.open() as stream:
                stream.seek(offset)
                positions = next(
                    (
                        json.loads(line.split("ranks=", 1)[1])
                        for line in stream
                        if "post-fusion positions" in line and "ranks=" in line
                    ),
                    None,
                )
            assert pages[-1][0]["SearchChunk"]["phase"] == "Final"
            trace = {
                kind: value
                for q, kind, value in map(
                    json.loads, (root / "trace").read_text().splitlines()
                )
                if q == query
            }
            first, final = [
                [h["title"] for h in page[0]["SearchChunk"]["hits"]]
                for page in (pages[0], pages[-1])
            ]
            ranks = [i for i, name in enumerate(final, 1) if name in labels]
            ideal = sum(1 / math.log2(2 + i) for i in range(min(10, len(labels))))
            count = sum(r <= 5 for r in ranks)
            if not labels:
                passed = not final and not first
            elif group == "visual":
                passed = 1 in ranks and count >= 4
            elif group == "filtered":
                passed = set(first) == set(labels) == set(final)
            else:
                passed = 1 in ranks
            rows.append(
                {
                    "query": query,
                    "group": group,
                    "language": lang,
                    "split": split,
                    "labels": labels,
                    "first": first,
                    "final": final,
                    "channels": trace,
                    "fusion_input_ranks": positions,
                    "first_samples_ms": [sample[0][1] for sample in samples],
                    "final_samples_ms": [sample[-1][1] for sample in samples],
                    "first_ms": pages[0][1],
                    "final_ms": pages[-1][1],
                    "ndcg10": sum(1 / math.log2(1 + r) for r in ranks) / ideal
                    if ideal
                    else None,
                    "recall5": count / len(labels) if labels else None,
                    "relevant_images5": count / 5 if group == "visual" else None,
                    "passed": passed,
                }
            )
        workers = [p for p in Path("/proc").glob("[0-9]*") if p.is_dir()]
        rss = 0
        for p in workers:
            try:
                if (
                    os.getpgid(int(p.name)) == daemon.pid
                    and p.joinpath("exe").resolve()
                    == (args.bin / "lixun-semantic-worker").resolve()
                ):
                    rss = int(
                        next(
                            x.split()[1]
                            for x in p.joinpath("status").read_text().splitlines()
                            if x.startswith("VmHWM:")
                        )
                    )
            except (OSError, StopIteration):
                continue
        return {
            "rows": rows,
            "worker_peak_kib": rss,
            "build_seconds": build_seconds,
            "embedded": sorted(indexed()),
            "model_log": [
                line
                for line in log.read_text().splitlines()
                if "loaded retrieval models" in line
            ],
        }
    finally:
        os.killpg(daemon.pid, signal.SIGTERM)
        try:
            daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            os.killpg(daemon.pid, signal.SIGKILL)
            daemon.wait()


if __name__ == "__main__":
    ap = argparse.ArgumentParser(description=__doc__)
    for arg in ("bin", "pets", "cache", "output"):
        ap.add_argument("--" + arg, type=Path, required=True)
    ap.add_argument("--split", choices=("dev", "holdout", "all"), default="dev")
    ap.add_argument(
        "--text-model",
        choices=("bge-small-en-v1.5", "bge-m3"),
        default="bge-small-en-v1.5",
    )
    ap.add_argument(
        "--image-model", choices=("clip-vit-b-32", "clip-vit-b-32-multilingual")
    )
    ap.add_argument("--allow-download", action="store_true")
    ap.add_argument("--repetitions", type=int, default=5)
    ap.add_argument("--skew", type=int, default=60)
    ap.add_argument("--gate", action="store_true")
    args = ap.parse_args()
    if args.repetitions < 1 or args.skew < 0:
        ap.error("repetitions must be positive and skew nonnegative")
    with tempfile.TemporaryDirectory(prefix="lixun-mixed-") as temp:
        report = evaluate(args, Path(temp))
    summary = []
    for group, lang in sorted({(r["group"], r["language"]) for r in report["rows"]}):
        rows = [
            r for r in report["rows"] if (r["group"], r["language"]) == (group, lang)
        ]
        metrics = {}
        for key in ("ndcg10", "recall5", "relevant_images5"):
            values = [r[key] for r in rows if r[key] is not None]
            metrics[key] = statistics.mean(values) if values else None
        for phase in ("first", "final"):
            times = sorted(t for r in rows for t in r[phase + "_samples_ms"])
            metrics[phase + "_p95_ms"] = times[math.ceil(0.95 * len(times)) - 1]
        summary.append(dict(group=group, language=lang, **metrics))
    report.update(summary=summary, skew=args.skew, repetitions=args.repetitions)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    for row in report["rows"]:
        print(row["query"], row["passed"], row["final"][:5])
    if args.gate:
        expected_image = args.image_model or "clip-vit-b-32-multilingual"
        assert any(
            "text=" + args.text_model + " " in line
            and "image=" + expected_image + " " in line
            for line in report["model_log"]
        ), "model selection mismatch"
        assert report["worker_peak_kib"] > 0
        gates = {"visual", "text", "exact", "filtered", "absent", "category"}
        if args.text_model == "bge-m3":
            gates.add("cross-language")
        for row in report["rows"]:
            if row["group"] == "visual":
                assert (
                    row["fusion_input_ranks"]
                    and {"classify_query", "search_text", "search_image"}
                    <= row["channels"].keys()
                ), "missing channel diagnostics"
            if row["group"] in gates and not row["passed"]:
                raise SystemExit("quality gate failed for " + row["query"])
