# untitled-brawler engineering standards

## Git Rules

- NEVER add attribution lines to git comments. Do not ever include Co-Authored-By or Claude-Session in git commits

## Style Rules

When writing prose, follow these rules:

- Words are expensive. Be succinct but clear. Do not be circuitous.
- Do not format paragraphs to an arbitrary max-width. Let lines wrap.
- NEVER use em dash or en dash characters.
- Never use special characters that can't be typed by a standard qwerty keyboard.
- Never use parentheticals in titles or headings. They can usually just be separate sentences
- Write the corrected state, never the correction. Never explain what an earlier version did wrong, what you removed, or why a rejected approach fails. A reader who never saw the earlier version must not be able to tell one existed.
- Cut context for anything that did not exist before the change and does not exist after it. e.g. "This does not write to the git directory" when nothing ever wrote there.
- A section must earn its place. Before writing one, ask whether a reader who already has the code and its docs would act differently for having read it. If not, cut it. Do not restate what another doc says, link to it. Do not write steering aimed at whoever is implementing right now. Do not invent troubleshooting for failures nobody has hit. Do not describe what is not built yet. An agent facing doc records what to run and what to observe.

## Coding Principles

- Our goal is to write efficient, maintainable, scalable, elegant code. This means:
  - We understand that words are expensive and context is limited. We don't do things (like write long comments, massive functions, huge files) that make it hard for future developers to digest or keep in context.
  - We look for opportunities to re-use code or to reshape existing code in a maintainable way that achieves our goal rather than writing new, possibly redundant code.
  - Are weary of inventing new types. We validate that there isn't already a type that could support our use case.
  - We consider systems' architecture and how our changes adhere to it. We don't add code that degrades our architecture for the sake of easiness, we take the approach that maintains the architecture and adheres to its explicit and implicit boundaries

## Testing

### Rust test modules

Unit tests live in a sibling file, never inline in the implementation file.

- `foo.rs` declares `#[cfg(test)] #[path = "foo_tests.rs"] mod tests;` and the tests live in `foo_tests.rs`.
- `mod.rs` and `lib.rs` declare `#[cfg(test)] mod tests;` and the tests live in `tests.rs` beside them.

### What To Test

Test what can actually be wrong: rules and predicates, state transitions, invariants, determinism, edge cases, and error paths.

Do not test type construction, derived traits, plain serde round trips of pure data, or field access. The compiler and the derives already guarantee those. Do not test Bevy composition either: plugin registration, run condition gating, and state defaults are wiring, not logic. Skip a test that only re-checks something a runtime fail-fast already catches; test the valid-but-wrong case instead.

A serde test earns its place only where there is custom serialization or a wire format that must stay stable. A plan enumerating "parses to X / round-trips" scenarios does not override this, skip them anyway. When unsure whether a test is wanted here, propose it rather than writing it.

## Comments

Comment the non-obvious why: placement rationale, invariants, tradeoffs, why not the obvious alternative. Never restate what the adjacent line already says. Declarative APIs are self-documenting, so do not narrate `run_if`/`resource_exists` wiring or an `extend`/`insert`/`push`.

## Git Review Method

This project follows a stacked, atomic code review method for changes.

`git review` must be used for all commits.
`git review` principles must be followed when planning or implementing changes.

Git review core principle: The commit is the unit of review. “Create small, self-contained commits. Each commit becomes a reviewable PR/MR, stacked on the previous one.”

1. Make small, coherent, self-contained commits.
2. Each commit must represent one conceptual change.
3. Each commit must leave the repository buildable/testable.
4. Include tests with the behavior they validate.
5. Order commits according to their dependencies.
6. Optimize the commit stack for a human reviewer, not for implementation convenience.
7. Do not mix unrelated refactors, formatting, cleanup, or behavior changes.
8. Before committing, verify the commit can be understood and reviewed independently.
9. Treat the stack as a sequence of reviewable changes, not merely implementation history.
10. When responding to review feedback, identify which commit owns the issue.
11. Correct earlier commits with `git commit --fixup <commit>`.
12. Prefer fixups over amending existing commits.
13. Never move a fix into the newest commit merely because it is convenient.
14. Avoid merge commits; maintain a linear stack and rebase when necessary.
15. Use `git review` to synchronize the entire stack with remote PRs.
16. When an earlier PR merges, allow the remaining stack to be rebased onto the new base.
17. Do not squash the stack merely because the final implementation works.
18. Commit messages should describe the conceptual change and its purpose.
19. Before submitting/updating review, inspect the resulting commit stack and ensure each PR tells a coherent story.
20. If a change cannot reasonably be represented as a self-contained commit, reconsider the implementation decomposition before proceeding.

When deciding where a change belongs, ask:

> "What conceptual change does this modification belong to?"
> not:
> "What commit am I currently working on?"

When deciding whether a commit is ready, ask:

> "Could a competent engineer review and validate this change without needing to understand the commits that follow it?"
