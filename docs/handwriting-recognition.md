# Handwriting recognition: model survey

Research for the ROADMAP item "Handwriting recognition: local, multi-language model" (and the
handwriting-aware search that depends on it). Surveyed 2026-11-01. Nothing here has been tested
on real Cera ink or on the tablet yet.

Constraints: runs locally, under 1 GB, multi-language, fits the budgets in DESIGN.md §10
(Tab S9, 8 GB shared RAM, Snapdragon 8 Gen 2 CPU, Adreno 740 under Mesa).

## Summary

There is no good open, multi-language **online** (stroke-based) recogniser. The practical route
is to group strokes into lines from their geometry, render each line as a clean image and run a
small **offline** (image-based) line recogniser on it. Because Cera has the exact strokes, no
text-detection model is needed.

Recommended default: Kiessling's kraken PP-OCRv6 line recognisers, small (14 MB) or tiny
(3 MB), as ONNX. They cover 44 languages in 10 scripts, are trained on handwriting and print,
and are Apache-2.0.

## Online (stroke-based) models

None are usable.

- **Google ML Kit Digital Ink Recognition**: the reference for this job. 300+ languages,
  25+ writing systems, about 20 MB per language, about 100 ms per line on a phone. Closed
  source, Android and iOS only, so not on Fedora.
- **InkFM** (Google, March 2025): a foundation model for full pages of online ink. It recognises
  text in 28 scripts and maths, and segments lines. The paper says Google plans to release it,
  but I found no weights.
- **InkSight Small-p** (Google, 340M parameters): converts *photos* of handwriting into digital
  ink, with word-level recognition as a side task. It goes the wrong way for Cera and needs
  TensorFlow/Keras.
- **Zinnia, hanzi_lookup** and similar: single Chinese and Japanese characters only.
- **MyScript iink**: commercial and proprietary. Not considered.

Other open-source note apps (Xournal++, Rnote, Saber) have no text recognition to borrow.

## Offline (image-based) candidates under 1 GB

### Dedicated line recognisers (small, fast)

| Model | Size | Languages | Handwriting | Licence |
|---|---|---|---|---|
| kraken PP-OCRv6 tiny / small / medium | 0.7M / 3.2M / 15.8M params; 3 / 14 / 64 MB ONNX | 44 languages, 10 scripts: Arabic, Armenian, Cyrillic, Ethiopic, Georgian, Greek, Hebrew, Latin, Malayalam, Syriac. No Chinese, Japanese or Korean | Trained on handwritten and printed lines. Character error rate 8.7% / 5.4% / 3.9% on its test set (micro-average; macro 11.0% / 6.9% / 4.9%) | Apache-2.0 |
| Official PP-OCRv6 tiny / small / medium (Baidu) | 1.5M / 7.7M / 34.5M params (the HF card says 19M for medium) | Chinese, English, Japanese and 46 Latin-script languages in one model | Handwritten-English score 39.3 / 61.1 / 67.8 and handwritten-Chinese 40.1 / 57.6 / 62.1 on Baidu's benchmark: weak | Apache-2.0 |
| PP-OCRv5 per-language models | about 7–9 MB each (mobile); 16 MB mobile / 81 MB server for the Chinese, English and Japanese model | 40+ languages, including Korean, Cyrillic, Arabic, Devanagari, as separate models | Stated strength for handwritten Chinese and English | Apache-2.0 |
| TrOCR small / base (Microsoft) | 62M / 334M params | English only (IAM) | Good on English | MIT |

Notes on kraken PP-OCRv6 (Benjamin Kiessling, ALMAnaCH / Inria, released 2026-08-04):

- Input: RGB line image of any width, resized to 96 px high, normalised as `(x/255 - 0.5) / 0.5`.
  Output: `(batch, time, 1623)` softmax probabilities with the CTC blank at index 0. Each time step
  covers 8 px. Greedy CTC decoding is enough.
- The training corpus is skewed towards English, French, German, Latin and Dutch, much of it
  historical. Accuracy on modern casual handwriting is unknown, and low-resource languages may
  need fine-tuning.
- It transcribes diplomatically, keeping long s, ligatures, combining diacritics and historical
  spelling. Search needs a normalisation step (NFKC, mapping long s to s, and so on).
- The original weights are kraken safetensors on Zenodo. SquiddleOCR on Hugging Face has ONNX
  conversions (`inference.onnx` + `dict.txt` per model).

### Small vision-language models (heavy)

| Model | Size | Languages | Handwriting | Licence |
|---|---|---|---|---|
| PaddleOCR-VL-1.6 (0.9B, May 2026) | about 0.5 GB Q8 / 0.3 GB Q4 GGUF, plus an image encoder (mmproj) of about 0.9 GB in bf16 | 109 languages (the family's figure) | PaddleOCR-VL v1 paper: edit distance 0.118 on English and 0.034 on Chinese handwriting (Ocean-OCR-Bench) | Apache-2.0 |
| OvisOCR2 (0.8B, July 2026, from Qwen3.5-0.8B) | about 1.7 GB in bf16; quantised versions exist | Not stated | Best on its own in-house handwriting subset: 72.28, against 67.66 for PaddleOCR-VL-1.6 and 69.58 for GLM-OCR | Apache-2.0 |
| LightOnOCR-2 (1B) | about 2 GB in bf16 | Multilingual | Its paper lists handwriting as a limitation | (not checked) |
| Florence-2 base / large | 0.23B / 0.77B | Mostly English | General-purpose; not built for handwriting | MIT |

PaddleOCR-VL support was merged into llama.cpp in February 2026 (build b8110), which also runs on
the tablet through Vulkan. Even so, these models are a poor fit for Cera:

- About 1 GB of RAM or more while running, and seconds per page on the tablet's CPU.
- The OmniHandwritingOCR benchmark (CIKM 2026) found that generative models "hallucinate
  plausible corrections" that the ink does not support. That is bad for a search index.
- Their strengths (tables, formulas, layout) don't matter for line-by-line ink.

They could only be an opt-in "transcribe page" add-on, not the indexer.

## Proposed design

ROADMAP lists recognition as an add-on, so the model and the runtime ship with the add-on and
cost nothing when it isn't installed. Only line grouping and rendering are general enough to
live in the core crates.

1. **Line grouping from geometry**: cluster strokes into lines by their vertical overlap and
   reading order, in `core`, so it can be unit-tested.
2. **Rendering**: rasterise each line to 96 px high, black on white, in `render`. The canvas
   pipeline already does this kind of work.
3. **Recognition**: kraken PP-OCRv6 small by default, tiny as a low-memory option. Run it on a
   background thread that indexes pages after edits settle. Load the model lazily and drop it
   when idle.
4. **Index**: store the recognised text per page (a new `.cera` chunk or a vault-level index) and
   normalise it for search.
5. **Later**: Chinese, Japanese and Korean through official PP-OCRv6 or a PP-OCRv5 language model,
   as a separate optional download.

Runtime options in Rust:

- `ort` (ONNX Runtime bindings): mature and fast on aarch64, but adds a native library of about
  20 MB. The `oar-ocr` crate (0.9.x) already wraps PP-OCRv6 recognition on top of it.
- `tract`: pure Rust and lighter. I haven't checked that it supports every operation in
  PP-OCRv6 (the recogniser is a recurrence-free SVTR-style network).
- `candle`: has a TrOCR example. Only relevant if TrOCR is chosen.

## Open questions and next steps

- Measure accuracy on real Cera notes: export a few pages, render the lines and run tiny, small
  and medium. Character error rate per language.
- Measure peak RSS and time per line on the Tab S9 (CPU), with `ort` and with `tract`.
- Decide how to ship the model: in the Flatpak or as an optional download.
- Watch for an InkFM release and for any open port of an ML Kit-class online recogniser.

## Sources

- [SquiddleOCR: kraken PP-OCRv6 as ONNX](https://huggingface.co/storytracer/squiddleocr)
- [kraken PP-OCRv6 tiny on Zenodo](https://zenodo.org/records/21788403)
  ([small](https://zenodo.org/records/21788405), [medium](https://zenodo.org/records/21788410))
- [A free lunch? Adapting PP-OCRv6 for historical text recognition](https://arxiv.org/pdf/2609.20064)
- [PaddleOCR releases (PP-OCRv6)](https://github.com/paddlepaddle/PaddleOCR/releases)
- [PP-OCRv6 medium recogniser, ONNX](https://huggingface.co/PaddlePaddle/PP-OCRv6_medium_rec_onnx)
- [PP-OCRv5 on Hugging Face](https://huggingface.co/blog/baidu/ppocrv5)
- [PaddleOCR 3.0 technical report](https://arxiv.org/pdf/2507.05595)
- [PaddleOCR-VL-1.6](https://huggingface.co/PaddlePaddle/PaddleOCR-VL-1.6)
- [PaddleOCR-VL GGUF](https://huggingface.co/axteam/PaddleOCR-VL-GGUF)
- [PaddleOCR-VL paper](https://arxiv.org/pdf/2510.14528)
- [OvisOCR2 technical report](https://arxiv.org/html/2607.13639v1) and
  [weights](https://huggingface.co/ATH-MaaS/OvisOCR2)
- [LightOnOCR](https://arxiv.org/html/2601.14251v2)
- [OmniHandwritingOCR benchmark](https://arxiv.org/abs/2608.18586)
- [InkFM](https://arxiv.org/abs/2503.23081)
- [Representing online handwriting for recognition in large vision-language models](https://arxiv.org/pdf/2402.15307)
- [InkSight Small-p](https://huggingface.co/Derendering/InkSight-Small-p/blob/main/README.md?code=true)
- [ML Kit Digital Ink Recognition](https://developers.googleblog.com/2020/08/digital-ink-recognition-in-ml-kit.html)
  and [Android docs](https://developers.google.com/ml-kit/vision/digital-ink-recognition/android)
- [TrOCR small handwritten](https://huggingface.co/microsoft/trocr-small-handwritten)
- [oar-ocr crate](https://docs.rs/crate/oar-ocr)
