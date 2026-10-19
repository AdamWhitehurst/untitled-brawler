---
name: prose
description: "Always load after writing prose: documents, commits, mr's, code documentation, etc."
---

# Prose

## 1. Locate the prose

When the prose already lives in a document, a code comment, or another file, revise it in place and set `PROSE` to that path.

Otherwise write it to a scratch file outside the repository:

```sh
PROSE="${TMPDIR:-/tmp}/PROSE-$(git rev-parse --abbrev-ref HEAD | tr / -).md"
```

## 2. Style

- Remove any irrelevant sections/prose or referring to implementation steering. If prose is addressing that did not exist before and does not exist after, it is irrelevant. e.g. "This MR does not use dependency X" when dependency X was never used prior to this MR.
- Do not break lines to conform to a max-width. There is no max width rule. Text wraps
- Markdown pipe tables cannot exceed 720 width. Excessively large cell contents can be split out from the table and referred to.

## 3. Machine Check

```sh
command -v slop-cop >/dev/null && slop-cop check --llm-effort=off "$PROSE"
```

`slop-cop` prints a JSON report and exits 0 even when it finds violations. Read the report. Do not trust the exit code. Skip this check when `slop-cop` is absent.

Do not clear the report. It rates narrative prose, so some rules fire on structure the document requires:

- Fix every violation whose `counts_by_category` is `sentence-structure`.
- Ignore `bold-first-bullets`, `listicle-instinct`, and `colon-elaboration` in a labelled bullet list.
- Judge the rest one at a time. A violation is a prompt to reread the sentence, not an order to delete it.

Keep `--llm-effort=off`. The default is `auto`, which starts a nested Claude pass that costs minutes. The 42 client-side rules are the mechanical check this step wants. You are the semantic pass.
