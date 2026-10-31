#!/usr/bin/env python3
"""Builds the static documentation site from the repository Markdown.

Run from anywhere:

    python3 site/build.py
    python3 -m http.server -d site 8080

Only the Python standard library is used, so the site rebuilds without a
package manager or network access.
"""

from __future__ import annotations

import html
import pathlib
import re

ROOT = pathlib.Path(__file__).resolve().parent.parent
SITE = ROOT / "site"

PAGES = [
    ("README.md", "overview.html", "Project overview", "Guides"),
    ("AGENTS.md", "development.html", "Development guide", "Guides"),
    ("examples/inspect-ai-evals/README.md", "evals.html", "inspect-ai evals", "Guides"),
    ("docs/api.md", "api.html", "HTTP API", "Reference"),
    ("docs/schemas.md", "schemas.html", "Custom table schemas", "Reference"),
    ("docs/architecture.md", "architecture.html", "Architecture", "Architecture"),
    ("docs/alpha.md", "alpha.html", "Alpha readiness", "Operations"),
    ("docs/performance.md", "performance.html", "Performance review", "Operations"),
    ("docs/security.md", "security.html", "Security review", "Operations"),
    ("docs/verification.md", "verification.html", "Verification", "Operations"),
    ("docs/benchmark.md", "benchmark.html", "Benchmark plan", "Operations"),
    ("docs/research/gap-analysis.md", "gap-analysis.html", "Gap analysis", "Research"),
    ("docs/research/adr-custom-schemas.md", "adr.html", "Custom schemas decision", "Research"),
    ("docs/research/adr-custom-schemas-grounding.md", "grounding.html", "Custom schemas grounding", "Research"),
    ("docs/research/formal-verification-brief.md", "fv-brief.html", "Formal verification brief", "Research"),
    ("docs/research/rocq-of-rust-assessment.md", "rocq.html", "Rocq of Rust assessment", "Research"),
    ("docs/research/fv-implementation-report.md", "fv-report.html", "FV implementation report", "Research"),
]

PAGE_BY_MD = {pathlib.Path(source).name: output for source, output, _, _ in PAGES}
SECTION_ORDER = ["Guides", "Reference", "Architecture", "Operations", "Research"]


def page_link(url: str) -> str:
    if url.rstrip("/") == "examples/inspect-ai-evals":
        return "evals.html"
    if url.endswith(".md"):
        name = pathlib.Path(url).name
        if name in PAGE_BY_MD:
            return PAGE_BY_MD[name]
    return url


def slug(text: str) -> str:
    slugged = re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")
    return slugged or "section"


def inline(text: str) -> str:
    codes: list[str] = []

    def stash(match: re.Match[str]) -> str:
        codes.append(html.escape(match.group(1), quote=False))
        return f"\x00{len(codes) - 1}\x00"

    text = html.escape(text, quote=False)
    text = re.sub(r"`([^`]+)`", stash, text)
    text = re.sub(r"\*\*([^*]+)\*\*", r"<strong>\1</strong>", text)
    text = re.sub(
        r"\[([^\]]+)\]\(([^)]+)\)",
        lambda match: (
            f'<a href="{html.escape(page_link(match.group(2)), quote=True)}">'
            f"{match.group(1)}</a>"
        ),
        text,
    )
    return re.sub(
        r"\x00(\d+)\x00",
        lambda match: f"<code>{codes[int(match.group(1))]}</code>",
        text,
    )


def is_table_start(lines: list[str], index: int) -> bool:
    return (
        index + 1 < len(lines)
        and lines[index].strip().startswith("|")
        and re.fullmatch(r"\|[-: |]+\|", lines[index + 1].strip()) is not None
    )


def parse_table(lines: list[str], index: int) -> tuple[str, int]:
    header = [cell.strip() for cell in lines[index].strip().strip("|").split("|")]
    index += 2
    rows: list[list[str]] = []

    while index < len(lines) and lines[index].strip().startswith("|"):
        rows.append([cell.strip() for cell in lines[index].strip().strip("|").split("|")])
        index += 1

    head = "".join(f"<th>{inline(cell)}</th>" for cell in header)
    body = "".join(
        "<tr>" + "".join(f"<td>{inline(cell)}</td>" for cell in row) + "</tr>" for row in rows
    )
    return f"<table><thead><tr>{head}</tr></thead><tbody>{body}</tbody></table>", index


def is_block_start(line: str) -> bool:
    stripped = line.strip()
    return (
        not stripped
        or stripped.startswith("```")
        or stripped.startswith("#")
        or stripped == "---"
        or stripped.startswith(">")
        or re.match(r"^[-*] ", stripped) is not None
        or re.match(r"^\d+\. ", stripped) is not None
    )


def parse_list(lines: list[str], index: int) -> tuple[str, int]:
    ordered = re.match(r"^\d+\. ", lines[index].strip()) is not None
    items: list[list[str]] = []

    while index < len(lines):
        stripped = lines[index].strip()
        marker = re.match(r"^(?:[-*]|\d+\.) ", stripped)

        if marker:
            items.append([stripped[marker.end() :]])
            index += 1
            continue

        if not stripped:
            break

        if not lines[index].startswith("  ") and is_block_start(stripped):
            break

        if items:
            items[-1].append(stripped)
            index += 1
            continue

        break

    tag = "ol" if ordered else "ul"
    body = "".join(f"<li>{inline(' '.join(item))}</li>" for item in items)
    return f"<{tag}>{body}</{tag}>", index


def convert(markdown: str) -> str:
    lines = markdown.splitlines()
    output: list[str] = []
    index = 0

    while index < len(lines):
        line = lines[index]
        stripped = line.strip()

        if stripped.startswith("```"):
            language = stripped[3:].strip()
            index += 1
            code: list[str] = []
            while index < len(lines) and not lines[index].strip().startswith("```"):
                code.append(lines[index])
                index += 1
            index += 1
            code_html = html.escape("\n".join(code), quote=False)
            class_name = f' class="language-{language}"' if language else ""
            output.append(f"<pre><code{class_name}>{code_html}</code></pre>")
            continue

        heading = re.match(r"^(#{1,6}) (.*)$", stripped)
        if heading:
            level = len(heading.group(1))
            text = heading.group(2).strip()
            output.append(f'<h{level} id="{slug(text)}">{inline(text)}</h{level}>')
            index += 1
            continue

        if stripped == "---":
            output.append("<hr>")
            index += 1
            continue

        if not stripped:
            index += 1
            continue

        if is_table_start(lines, index):
            table, index = parse_table(lines, index)
            output.append(table)
            continue

        if re.match(r"^[-*] ", stripped) or re.match(r"^\d+\. ", stripped):
            listing, index = parse_list(lines, index)
            output.append(listing)
            continue

        if stripped.startswith(">"):
            output.append(f"<blockquote><p>{inline(stripped[1:].strip())}</p></blockquote>")
            index += 1
            continue

        paragraph: list[str] = [stripped]
        index += 1
        while index < len(lines) and not is_block_start(lines[index]) and not is_table_start(
            lines, index
        ):
            paragraph.append(lines[index].strip())
            index += 1
        output.append(f"<p>{inline(' '.join(paragraph))}</p>")

    return "\n".join(output)


def nav(current: str) -> str:
    groups: dict[str, list[tuple[str, str]]] = {section: [] for section in SECTION_ORDER}
    for _, output, title, section in PAGES:
        groups[section].append((output, title))

    parts = ['<a class="brand" href="index.html">TraceLake</a>']
    for section in SECTION_ORDER:
        if not groups[section]:
            continue
        parts.append(f'<p class="nav-section">{section}</p><ul>')
        for output, title in groups[section]:
            class_name = ' class="active"' if output == current else ""
            parts.append(f'<li><a{class_name} href="{output}">{title}</a></li>')
        parts.append("</ul>")

    return "\n".join(parts)


TEMPLATE = """<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} - TraceLake</title>
<link rel="stylesheet" href="assets/style.css">
</head>
<body>
<div class="layout">
<aside class="sidebar">
{nav}
</aside>
<main>
<article>
{content}
</article>
<footer>
<p>TraceLake documentation. Built from the repository Markdown by <code>site/build.py</code>.</p>
</footer>
</main>
</div>
</body>
</html>
"""

INDEX = """<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>TraceLake - trace database for LLM agents</title>
<link rel="stylesheet" href="assets/style.css">
</head>
<body>
<div class="layout">
<aside class="sidebar">
{nav}
</aside>
<main>
<article class="hero">
<p class="tag">alpha</p>
<h1>TraceLake</h1>
<p>A trace database for LLM agents. Object storage is the source of truth, SlateDB
is the hot write buffer, and compaction writes trace packs, typed record packs,
Parquet columns, bloom filters, and Tantivy indexes. Full traces, typed tables,
eval logs, search, and aggregation come from one engine.</p>
<h2>Start here</h2>
<div class="cards">
<a class="card" href="api.html"><strong>HTTP API</strong><span>Every endpoint, shape, limit, and status code.</span></a>
<a class="card" href="schemas.html"><strong>Custom tables</strong><span>Typed schemas, evolution rules, records, and queries.</span></a>
<a class="card" href="architecture.html"><strong>Architecture</strong><span>Storage layout, write and read paths, and the decisions behind them.</span></a>
<a class="card" href="evals.html"><strong>inspect-ai evals</strong><span>A real eval run through ingest and query.</span></a>
</div>
<h2>Run it</h2>
<pre><code>cargo run --release -p tracelake-server
cargo test --workspace
cargo run --release -p tracelake-bench
cargo run -p tracelake-server --example inspect_ai_demo</code></pre>
<p>The server writes to <code>./tracelake-data</code> by default. Set
<code>TRACELAKE_STORE</code> to <code>s3</code> or <code>gcs</code> for object
storage. There is no authentication; bind to loopback or a private network.</p>
<h2>Where the project stands</h2>
<p>Ingest, trace fetch, structural and full-text search, Parquet aggregation,
typed tables with evolution, inspect-ai eval ingest, deletes, retention, read
limits, and local or remote object storage are implemented. Open work is listed
in <a href="alpha.html">Alpha readiness</a>. Gaps against SmithDB and Brainstore
are in <a href="gap-analysis.html">Gap analysis</a>.</p>
</article>
<footer>
<p>TraceLake documentation. Built from the repository Markdown by <code>site/build.py</code>.</p>
</footer>
</main>
</div>
</body>
</html>
"""


def build() -> None:
    SITE.mkdir(exist_ok=True)
    (SITE / "assets").mkdir(exist_ok=True)

    for source, output, title, _ in PAGES:
        markdown = (ROOT / source).read_text(encoding="utf-8")
        document = TEMPLATE.format(
            title=html.escape(title),
            nav=nav(output),
            content=convert(markdown),
        )
        (SITE / output).write_text(document, encoding="utf-8")
        print(f"{source} -> site/{output}")

    (SITE / "index.html").write_text(
        INDEX.format(nav=nav("index.html")),
        encoding="utf-8",
    )
    print("site/index.html")


if __name__ == "__main__":
    build()
