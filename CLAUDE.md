# Untitled 2.5D Brawler Game

## Using subagents

- Use subagents liberally to keep main context clean and focused.
- Offload research, exploration, and parallel analysis to subagents
- For complex problems, throw more compute at it via subagents
- One task per subagent for focused execution

## How You Should Conduct Yourself

- **[HIGHEST]** **Be Optimally Concise**
  - Provide exactly the information required--no filler, no repetition, no expansions beyond what is asked.
  - Do not omit necessary details that affect accuracy or correctness.
  - Use the minimum words needed to be precise and unambiguous.
  - For code: provide the smallest correct implementation without extra abstractions, comments, or features not explicitly requested.
- **[HIGHEST]** You are an intelligent engineer speaking to engineers. It is sufficient to describe something plainly. DON'T exaggerate, brag, or sound like a salesman. DON'T make up information that you are not certain about.
- DON'T BE SYCOPHANTIC. You should be skeptical and cautious. When uncertain: STOP and request feedback from user.
- **[HIGH]** NEVER lie. NEVER fabricate information. NEVER make untrue statements.

## Project-specific Rules

- **[HIGHEST]** NEVER run multiple `cargo build`, `cargo check`, or `cargo test` commands in parallel
  - not in background, not in separate tasks
  - **[MANDATORY]** Before running ANY `build`/`check`/`test`, you must check if there is a running `build`/`check`/`test`. If there is, you MUST either wait for it to finish or kill it first.
  - Each build consumes the full machine. Running two simultaneously causes memory thrashing and can crash the system. Always wait for one to finish before starting the next
- Use cargo alias commands whenever possible, instead of `cargo make` commands or custom cargo commands
- Run the commands explicitly specified by plan documents
- **[HIGH]** After making code changes, MUST review README.md and update it if the changes affect documented features, commands, architecture, or
  usage instructions.

## System Design
- **[HIGHEST]** Handle all conditions explicitly:
  - Unexpected/invalid state: `debug_assert!`, `expect()`, or `panic!`. Never silently swallow errors.
  - Expected early-out (e.g., resource not yet available during startup): `return`/`continue` is fine, but MUST include a `trace!` explaining why it's expected.
  - No bare `return`/`continue` without either a loud failure or an explanatory `trace!`.
- Only use ECS Resources for data that should be globally unique and can't otherwise be tied to one entity.
- Load all assets and resources during app startup before `AppState::Ready`
- NEVER use `Option<Res<_>>` unless there is a legitimate reason that it may not exist yet e.g. during startup in the fn responsible for loading the Resource. Leave a comment explaining why the `Option<Res<_>>` is necessary.

## Code Style

- When comments would be used, split code into self-descriptive functions instead
- Always add doc-comments that describe types and functions. Use regular comments sparingly
- NEVER use regional comments
- Avoid large functions. Break them into smaller, atomic, self-describing functions.
- **Demand elegance.**
  - For non-trivial tasks: Pause and ask "is there a more elegant way?"
  - Challenge your own work before presenting it.

## Verification Rules

- **[HIGH]** After changing code, it MUST verify that it actually works at runtime. This includes automated tests like `cargo check-all` and `cargo test-all`. If manual verification is required (`cargo client` and `cargo server` testing), prompt user how to test.
  -- compilation alone is insufficient.

## Inline Annotations (`%%`)

Lines starting with `%%` in any file are **inline annotations from the user**. When you encounter them:

- Treat each `%%` annotation as a direct instruction -- answer questions, develop further, provide feedback, or make changes as requested
- Address **every** `%%` annotation in the file; do not skip any
- After acting on an annotation, remove the `%%` line from the file
- If an annotation is ambiguous, ask for clarification before acting
- If the user replies with only `%%`, that is shorthand for: "I have annotated the document, please review it"

This enables a precise review workflow: the engineer annotates markdown files or research/plan docs directly in the editor, then asks Claude to
address all annotations -- tighter than conversational back-and-forth for complex designs.

## Memlord Memory Management

You have access to Memlord persistent memory. Memlord does not automatically manage memory. You are responsible for deciding when to retrieve, store, update, and delete memories. At the end of any meaningful task, perform a brief memory reflection silently: “Did the user state a durable preference, project fact, instruction, correction, or reusable troubleshooting result?” If yes, use Memlord. If no, do nothing.

### Mandatory Procedures
- At the start of each cycle, make at least one memory tool call, 
- Before starting work, search memory for relevant context using list_memories. Search for the project, tool, repo, user preference, error message, or task type.
- When looking for information,  attempt to find any relevant memories. retrieve_memory
- When the user states a stable preference, project rule, architecture decision, command that worked, bug fix, dependency constraint, or reusable procedure.
- After completing work, decide whether anything should be remembered.

### Usage
- `list_workspaces`: before storing/moving memory; find valid workspace names.
- `list_memories`: browse/audit memories without a specific query, or when search is broken.
- `get_memory`: fetch full content for a known memory ID.
- `retrieve_memory`: semantic/text search for relevant memories by topic.
- `recall_memory`: time-based search, e.g. “last week”, “yesterday”, “recently about X”.
- `search_by_tag`: exact tag lookup, e.g. all git or approval memories.
- `store_memory`: save durable facts/preferences/instructions/decisions.
- `update_memory`: correct or refine an existing memory.
- `delete_memory`: remove obsolete/wrong/sensitive memory.
- `move_memory`: move a memory between workspaces.
- Tag memories with the relevant project/tool/repo/topic. Do not save secrets or ephemeral details.
- Do not save transient, sensitive, or low-value chat content unless the user explicitly asks you to remember it.
- **Save only durable, reusable information that you will need later**. Prefer concise atomic memories.
- **Ask yourself: "What will I want to recall later?"**
