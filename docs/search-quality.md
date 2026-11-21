# Mixed-corpus search quality

On the held-out topic, the default configuration returned a dog photo first and four dog photos in the first five results for each of the English, Russian and Chinese queries. The baseline returned one, one and zero respectively. The dog-care article ranked first in all three languages on both builds.

## Corpus and diagnosis

The benchmark starts a release daemon and worker with an empty temporary profile. It uses ten neutral filenames for five dog and five cat photographs from ten different breeds, two labelled screenshots, a scanned PDF, a text PDF, four substantive text documents and either six or sixty text distractors. The archive checksum and exact filenames are in the harness. OCR is disabled so the screenshot competes on its pixels. With sixty distractors, the backfill journal confirmed all 77 expected vector-eligible files, including every labelled photograph. The scanned PDF had no text vector. [Oxford-IIIT Pet Dataset](https://www.robots.ox.ac.uk/~vgg/data/pets/)

Cats were the development topic. The dog queries and associated relevance labels were reserved until the configuration was selected. Each animal appears in five different breed photographs. The three translations reuse those five photographs and represent one held-out topic. These scores describe that corpus.

On the development set, the original image channel already found 4/3/4 relevant cats in the first five for EN/RU/ZH, but the final page contained 2/2/1. The router classified all three as mixed. The body of every distractor mentioned both animal topics. The lexical and text-vector channels could both vote for a distractor, and tied lexical scores could erase the fused order when the daemon sorted again. Changing the fusion rules alone raised the counts to 4/3/4. Keeping those rules and changing only the image-query encoder raised them to 4/4/4. Bare `cats` still put a text distractor first. The alternative document model improved cross-language text retrieval on development without improving explicit photo retrieval.

## Held-out results

Each cell lists **relevant photos in the first five / nDCG@10**. The relevance fraction and Recall@5 are both the photo count divided by five because exactly five photos are relevant. The first row is the release build of source revision `e12d950`. Subsequent rows change one factor at a time.

| Configuration | English | Russian | Chinese | Correct first photo |
|---|---:|---:|---:|---:|
| Baseline | 1 / 0.339 | 1 / 0.272 | 0 / 0.219 | 1/3 |
| Revised fusion, original CLIP | 4 / 0.907 | 2 / 0.685 | 2 / 0.673 | 2/3 |
| Revised fusion, multilingual CLIP | 4 / 0.907 | 4 / 0.907 | 4 / 0.907 | 3/3 |
| Multilingual CLIP, BGE-M3 documents | 4 / 0.907 | 4 / 0.907 | 4 / 0.907 | 3/3 |

In the selected run the English first page contained ten meeting notes. The visual channel placed a dog first, a screenshot of the word “dogs” second and four more dogs at positions 3–6. The final page preserved that order. All five dogs appeared in the final top ten for every translation. The same four-of-five threshold passed with six distractors. The screenshot remains a false positive. A request for elephant photos also returned neighbours even though the corpus contained no elephants.

The table below uses the selected default on the larger corpus. Times are end-to-end socket latencies in milliseconds. Each query ran five times. Reported p95 uses the nearest rank, so for one query with five samples it is the maximum. Empty relevance sets have no defined nDCG or recall.

| Intent | Language | nDCG@10 | Recall@5 | First page p95 | Final page p95 |
|---|---|---:|---:|---:|---:|
| absent | EN | — | — | 0.6 | 0.6 |
| category | EN | — | — | 0.7 | 14.2 |
| cross-language | ZH | 0.333 | 0.00 | 0.6 | 14.4 |
| exact | EN | 1.000 | 1.00 | 1.0 | 15.4 |
| filtered | EN | 1.000 | 1.00 | 0.5 | 0.5 |
| text | EN | 1.000 | 1.00 | 1.2 | 13.1 |
| text | RU | 1.000 | 1.00 | 1.8 | 17.6 |
| text | ZH | 1.000 | 1.00 | 1.3 | 15.6 |
| unanswerable-visual | EN | — | — | 1.1 | 12.9 |
| visual | EN | 0.907 | 0.80 | 1.5 | 17.1 |
| visual | RU | 0.907 | 0.80 | 0.9 | 15.1 |
| visual | ZH | 0.907 | 0.80 | 0.9 | 14.9 |

The exact filename stayed first. The quoted phrase and the field query with an exclusion returned exactly their labelled document on both pages. The baseline added unrelated final hits to both queries despite a relevant first hit, which nDCG alone obscures. The empty category and unmatched field returned empty pages. The default English document encoder ranked the hospital document seventh for a Chinese query. BGE-M3 ranked it first.

## Resources and model choice

These are local measurements on a warm model cache, with a fresh vector index for every run. Build time covers daemon startup through completed backfill, excluding network downloads. Peak worker RSS comes from Linux VmHWM. Latency p95 aggregates all 65 held-out requests per configuration. Local results include filesystem and model caching effects.

| Configuration | Peak RSS (MiB) | Startup + 77 embeddings (s) | First p95 (ms) | Final p95 (ms) |
|---|---:|---:|---:|---:|
| Baseline | 1305 | 3.30 | 0.9 | 21.1 |
| Revised fusion, original CLIP | 1352 | 3.28 | 1.2 | 19.1 |
| Revised fusion, multilingual CLIP | 1301 | 3.73 | 1.2 | 15.4 |
| Multilingual CLIP, BGE-M3 documents | 2623 | 5.55 | 1.2 | 35.4 |

The default keeps the 384-dimensional int8 BGE-small document encoder and the 512-dimensional CLIP vision encoder. It changes the image-query tower to the distilled multilingual model with mean pooling, learned projection and normalization. Existing vision vectors stay usable without re-embedding. The earlier English query tower remains selectable. BGE-M3 uses a separate 1024-dimensional store and requires backfill. It remains opt-in because it roughly doubled peak worker memory without improving dog retrieval. The comparison used preservation of the existing memory footprint as the desktop budget. [Multilingual CLIP model card](https://huggingface.co/sentence-transformers/clip-ViT-B-32-multilingual-v1)

Model revisions recorded in the benchmark cache follow. The multilingual query tower used `onnx/model_quint8_avx2.onnx` and `2_Dense/model.safetensors`.

| Model repository | Snapshot |
|---|---|
| `Qdrant/bge-small-en-v1.5-onnx-Q` | `52398278842ec682c6f32300af41344b1c0b0bb2` |
| `Qdrant/clip-ViT-B-32-vision` | `e0c24ed0fa57fa3e4f97f30de74c51d944036ace` |
| `Qdrant/clip-ViT-B-32-text` | `48ca1db27cb4063eb311ec2aa7f087a808112876` |
| `sentence-transformers/clip-ViT-B-32-multilingual-v1` | `58edf8cada9e398793dca955574a48cbb7f18be2` |
| `BAAI/bge-m3` | `5617a9f61b028005a4858fdac845db406aefb181` |

## Reproduce and inspect

Install Python with Pillow and ReportLab, then run the following from the checkout. Pass the cache directory **containing** the `fastembed` subdirectory. The archive is verified before individual fixture files are read.

```sh
cargo build --release --locked -p lixun-daemon -p lixun-semantic-worker -p lixun-cli
curl -fL https://www.robots.ox.ac.uk/~vgg/data/pets/data/images.tar.gz -o /tmp/lixun-pets.tar.gz
python tests/search-quality/mixed.py --bin target/release --allow-download \
  --cache /tmp/lixun-quality-models --pets /tmp/lixun-pets.tar.gz \
  --split all --skew 60 --output /tmp/lixun-quality.json --gate
```

Use `--skew 6` for the balanced corpus, `--image-model clip-vit-b-32` for the original query encoder, and `--text-model bge-m3` for multilingual documents. Ordinary CI tests exercise ordering without model downloads. The full-model workflow runs on retrieval-model and fusion pull requests and gates package publication. It checks first rank, photo precision, article rank, filenames and both constrained pages. Ambiguous requests, absent visual concepts and cross-language text with the English encoder are reported diagnostics. BGE-M3's cross-language check is enforced when that model is requested.

The [recorded measurements](../tests/search-quality/measurements.json.gz) contain development and held-out labels, per-channel rankings, the classifier decision, both socket pages and latency samples. The current build also records the channel ranks of each fused hit before downstream scoring as triples of lexical, text and image rank (zero means absent). The baseline binary predates that trace. For example, `gzip -dc tests/search-quality/measurements.json.gz | jq '.["final-selected"].rows[0]'` shows the entire English dog query.
