#!/usr/bin/env python3
"""Generate a deterministic large text corpus for compression benchmarks."""

from __future__ import annotations

import argparse
from pathlib import Path


NOUNS = [
    "archive",
    "buffer",
    "chapter",
    "checksum",
    "corpus",
    "dictionary",
    "document",
    "engine",
    "entropy",
    "frame",
    "header",
    "index",
    "journal",
    "ledger",
    "library",
    "message",
    "model",
    "packet",
    "paragraph",
    "pattern",
    "pipeline",
    "reader",
    "record",
    "report",
    "sample",
    "segment",
    "stream",
    "symbol",
    "table",
    "window",
]

VERBS = [
    "adapts",
    "balances",
    "compares",
    "compresses",
    "copies",
    "counts",
    "detects",
    "encodes",
    "filters",
    "flushes",
    "groups",
    "indexes",
    "measures",
    "packs",
    "predicts",
    "reads",
    "reduces",
    "restores",
    "scans",
    "streams",
    "tracks",
    "validates",
    "writes",
]

ADJECTIVES = [
    "adaptive",
    "binary",
    "careful",
    "compact",
    "deterministic",
    "efficient",
    "fast",
    "layered",
    "local",
    "measured",
    "parallel",
    "portable",
    "practical",
    "predictable",
    "randomized",
    "repeatable",
    "resilient",
    "structured",
    "textual",
    "verified",
]

ADVERBS = [
    "accurately",
    "carefully",
    "consistently",
    "directly",
    "eagerly",
    "efficiently",
    "locally",
    "quickly",
    "safely",
    "steadily",
]

CONNECTORS = [
    "after",
    "because",
    "before",
    "while",
    "when",
    "where",
    "as",
    "until",
]

PUNCTUATION = [".", ".", ".", ".", ";", ":"]


class Lcg:
    def __init__(self, seed: int) -> None:
        self.state = seed & 0xFFFFFFFFFFFFFFFF

    def next(self) -> int:
        self.state = (6364136223846793005 * self.state + 1442695040888963407) & 0xFFFFFFFFFFFFFFFF
        return self.state

    def range(self, start: int, stop: int) -> int:
        return start + self.next() % (stop - start)

    def choice(self, values: list[str]) -> str:
        return values[self.range(0, len(values))]


def sentence(rng: Lcg, section: int) -> str:
    templates = [
        "the {adj} {noun} {verb} the {adj2} {noun2} {adv}",
        "{connector} the {noun} {verb}, the {adj} {noun2} {verb2} {adv}",
        "a {adj} {noun} and a {adj2} {noun2} {verb} the section {number} record",
        "engineers {verb} each {adj} {noun} while the {noun2} {verb2} {adv}",
    ]
    template = rng.choice(templates)
    text = template.format(
        adj=rng.choice(ADJECTIVES),
        adj2=rng.choice(ADJECTIVES),
        adv=rng.choice(ADVERBS),
        connector=rng.choice(CONNECTORS),
        noun=rng.choice(NOUNS),
        noun2=rng.choice(NOUNS),
        number=section * 10_000 + rng.range(100, 999),
        verb=rng.choice(VERBS),
        verb2=rng.choice(VERBS),
    )
    return text.capitalize() + rng.choice(PUNCTUATION)


def paragraph(rng: Lcg, section: int) -> bytes:
    sentence_count = rng.range(5, 10)
    sentences = [sentence(rng, section) for _ in range(sentence_count)]
    heading = f"Section {section:05d} | stream={rng.next():016x}"
    return (heading + "\n" + " ".join(sentences) + "\n\n").encode("utf-8")


def generate(output: Path, size_mib: int) -> None:
    target = size_mib * 1024 * 1024
    rng = Lcg(seed=0x1770_C0DE)
    output.parent.mkdir(parents=True, exist_ok=True)

    written = 0
    section = 1
    with output.open("wb") as handle:
        while written < target:
            block = paragraph(rng, section)
            chunk = block[: target - written]
            handle.write(chunk)
            written += len(chunk)
            section += 1


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=Path("benchmarks/sample-large.txt"))
    parser.add_argument("--size-mib", type=int, default=40)
    args = parser.parse_args()

    generate(args.output, args.size_mib)
    print(f"Wrote {args.size_mib} MiB to {args.output}")


if __name__ == "__main__":
    main()
