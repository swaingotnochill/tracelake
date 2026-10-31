# Documentation site

Static documentation generated from the repository Markdown. There are no
dependencies beyond Python 3.

```bash
python3 site/build.py
python3 -m http.server -d site 8080
```

Then open `http://localhost:8080`.

`build.py` converts the Markdown subset the repository uses: headings,
paragraphs, fenced code, tables, lists, blockquotes, inline code, bold, and
links. Links to Markdown pages are rewritten to the generated HTML pages. Add a
page by adding it to `PAGES` in `build.py`.

Generated pages:

- `index.html`
- `overview.html`, `development.html`, `evals.html`
- `api.html`, `schemas.html`
- `architecture.html`
- `alpha.html`, `performance.html`, `security.html`, `verification.html`, `benchmark.html`
- `gap-analysis.html`, `adr.html`, `grounding.html`, `fv-brief.html`, `rocq.html`, `fv-report.html`
