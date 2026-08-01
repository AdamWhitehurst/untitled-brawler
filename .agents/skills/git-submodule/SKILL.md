---
name: git-submodule
description: Manage git submodules under `git/` for dependency source access. Triggers when (1) researching, understanding, or debugging a dependency's behavior — always search the local submodule before web/docs, (2) encountering a dependency whose source isn't yet under `git/` — clone it as a submodule before falling back to web search, (3) adding a new crate that needs a path-dep or patch override, (4) cleaning up submodules no longer needed. Source code is the single source of truth; prefer reading it over external documentation.
---

# Git Submodule

Manage git submodules under `git/` to enable fast local-source searches and optional path-based Cargo dependencies.

## Prioritize local sources

Before consulting external documentation or performing web searches, search the relevant submodule under `git/` for:

- Source code and API implementations
- Internal documentation and examples
- Tests that demonstrate usage patterns

Source code is the single source of truth. External docs are for high-level concepts, and only after local search is exhausted.

Cross-reference with `Cargo.toml` — a submodule may be ahead of or behind the version actually resolved by Cargo. Verify the version in use before drawing conclusions from submodule source.

## Clone an existing submodule

After a fresh clone or a pull that introduces new submodules:

```bash
git submodule update --init --recursive
```

## Add a submodule for searching only

When investigating a dependency whose source isn't under `git/`, clone it as a submodule rather than falling back to web search:

```bash
git submodule add <repo-url> git/<name>
```

Then exclude the path from the Cargo workspace so it isn't compiled as a workspace member:

```toml
# Cargo.toml
[workspace]
exclude = [
  "git/<name>",
  # ...existing entries (keep alphabetical)
]
```

Commit `.gitmodules`, the new submodule pointer, and the `Cargo.toml` change together.

## Add a submodule as a project dependency

Two patterns, depending on intent:

**Path dependency** — use a local checkout instead of the published crate:

```toml
[workspace.dependencies]
<name> = { path = "git/<name>", features = [...] }
```

Note: if the crate lives in a subdirectory of the repo, include it in the path (e.g., `path = "git/lightyear/lightyear"`).

**Patch crates-io** — override a transitive dependency without editing every dependent:

```toml
[patch.crates-io]
<name> = { path = "git/<name>" }
```

In both cases, also add `"git/<name>"` to `[workspace] exclude` so the submodule isn't pulled into the workspace.

## Remove a submodule

To fully remove a submodule (no longer needed for search or as a dependency):

```bash
git submodule deinit -f git/<name>
git rm -f git/<name>
rm -rf .git/modules/git/<name>
```

Then remove all references from `Cargo.toml`:

- `[workspace] exclude` entry
- `[workspace.dependencies]` path entry (if any)
- `[patch.crates-io]` entry (if any)

Commit the submodule removal and the `Cargo.toml` cleanup together.

## Verification

After adding, updating, or removing a submodule, run `cargo check-all` to confirm the workspace still resolves.
