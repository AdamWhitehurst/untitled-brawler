---
name: debug
description: Systematic hypothesis-driven debugging for diagnosing root causes. Use when the user reports a bug, unexpected behavior, or runtime issue and wants structured investigation. Propose hypothesis, instrument code to test it, validate or reject, iterate until root cause is found, then fix and clean up.
---

# Debug

Systematic root-cause debugging via hypothesis testing. Never guess-and-check blindly — form a theory, instrument it, prove or disprove, repeat.

## Invocation

Accept a bug description from the user. If insufficient, ask:
- What is the expected behavior?
- What is the actual behavior?
- When did it last work / what changed?

## Process

### Phase 1: Gather Evidence

Before hypothesizing, collect raw facts:
1. **Read any directly mentioned files first:**
   - If the user mentions specific files (tasks, docs, JSON), read them FULLY first
   - **IMPORTANT**: Use the Read tool WITHOUT limit/offset parameters to read entire files
   - **CRITICAL**: Read these files yourself in the main context before spawning any sub-tasks
   - This ensures you have full context before decomposing the research
2. **Analyze and decompose the problem**:
   - Check `git log --oneline -10` and `git diff` for recent changes
   - Check build output / runtime logs if available
   - Break down the user's explanation into composable research areas
   - Take time to ultrathink about the underlying patterns, connections, and architectural implications the user might be seeking
   - Identify specific components, patterns, or concepts to investigate
   - Create a research plan using TodoWrite to track all subtasks
   - Consider which directories, files, or architectural patterns are relevant

2. **Spawn parallel sub-agent tasks for comprehensive research:**
   - CRITICAL: After understanding the research question, you do NOT do the research yourself. You MUST spawn sub-agents to research different aspects of the question
   - Create multiple Task agents to research different aspects concurrently
   - We now have specialized agents that know how to do specific research tasks:

   **CRITICAL: SUB-AGENT DIRECTION:**
   - **Only search and report**: Sub-agents should ONLY search the codebase and return findings in their response
   - **YOU synthesize**: Only YOU (the main research agent) create the final research document in doc/bug/
   - **Use "find and report back" language**: When prompting sub-agents, use phrases like:
     - "Find and report back on..."
     - "Search for and describe..."
     - "Locate and return information about..."
   - **Do NOT use "document" language**: Avoid phrases like "document this" or "create documentation" which may trigger file creation

   **For codebase research (PRIMARY AGENTS FOR RESEARCH):**
   - Use the **codebase-locator** agent to find WHERE files and components live
   - Use the **codebase-analyzer** agent to understand HOW specific code works (without critiquing it)
   - Use the **codebase-pattern-finder** agent to find examples of existing patterns (without evaluating them)

   **IMPORTANT**: All agents are documentarians, not critics. They will describe what exists without suggesting improvements or identifying issues.

   **For web research:**
   - Use the **web-search-researcher** agent for external documentation and resources
   - IF you use web-research agents, instruct them to return LINKS with their findings, and please INCLUDE those links in your final report

   **WHEN TO USE SPECIALIZED DOMAIN AGENTS:**

   The codebase research agents above are your PRIMARY tools for documentation. However, when research reveals specific implementation needs OR when the user's question explicitly relates to a specialized domain, you may ALSO use domain specialists to provide additional context.

   **CRITICAL GUIDELINES FOR USING DOMAIN SPECIALISTS:**
   1. **Default to codebase research agents first**: Always start with codebase-locator, codebase-analyzer, and codebase-pattern-finder
   2. **Add domain specialists when**:
      - The research question relates to their domain 
   3. **Domain specialists search and report in research mode**:
      - Remind them they are FINDING and REPORTING on EXISTING implementations
      - They should NOT suggest improvements or identify issues
      - They should NOT create any files - only return findings in their response
      - They should focus on "what exists" and "how it works"
   4. **Run agents in parallel when they research different aspects**:
      - Example: codebase-locator (find files) + game-developer (understand game patterns)
      - Example: codebase-analyzer (code details) + rust-engineer (Rust idioms)
   5. **Synthesize findings from all agents** into coherent research document

3. **Wait for all sub-agents to complete and synthesize findings:**
   - IMPORTANT: Wait for ALL sub-agent tasks to complete before proceeding
   - Compile all sub-agent results (both codebase and doc findings)
   - Prioritize live codebase findings as primary source of truth
   - Use doc/ findings as supplementary historical context
   - Connect findings across different components
   - Include specific file paths and line numbers for reference
   - Highlight patterns, connections, and architectural decisions
   - Answer the user's specific questions with concrete evidence

4. **Generate or Update research document:**
   - Document all relevant findings
   - Use the metadata gathered in step 4
   - Structure the document with YAML frontmatter followed by content:
     ```markdown
     ---
     date: [Current date and time with timezone in ISO format]
     researcher: [Researcher name from doc status]
     git_commit: [Current commit hash]
     branch: [Current branch name]
     repository: [Repository name]
     topic: "[User's Bug]"
     tags: [bug, codebase, relevant-component-names]
     status: complete
     last_updated: [Current date in YYYY-MM-DD format]
     last_updated_by: [Researcher name]
     ---

     # Bug: [Bug Summary]

     **Date**: [Current date and time with timezone from step 4]
     **Researcher**: [Researcher name from doc status]
     **Git Commit**: [Current commit hash from step 4]
     **Branch**: [Current branch name from step 4]
     **Repository**: [Repository name]

     ## User's Prompt
     [Original user prompt]

     ## Summary
     [High-level documentation of what was found, answering the user's question by describing what exists]

     ## Investigation

     ### [Component/Area 1]
     - Description of what exists ([file.ext:line](link))
     - How it connects to other components
     - Current implementation details (without evaluation)

     ### [Component/Area 2]
     ...

     ## Code References
     - `src/systems/movement.rs:45` - Movement system implementation
     - `src/components/physics.rs:23-67` - Physics component definitions
     - `assets/config/gameplay.ron:12` - Game configuration values

     ## Architecture Documentation
     [Current patterns, conventions, and design implementations found in the codebase]

     ## Hypotheses
     [Sequential H1, H2, H3... — each with Hypothesis, Prediction, Test, Decision fields]

     ## Fixes
     [Sequential F1, F2, F3... — each linked to a hypothesis, with Root Cause, Fix, Risk, Decision fields]

     ## Solutions
     [filled in later]
     ```

### Phase 2: Hypothesize

Propose a **single root cause hypothesis** using sequential IDs: **H1**, **H2**, **H3**, etc.

```
### H[N]: [Short descriptive title]

**Hypothesis:** [What you believe is wrong and why]
**Prediction:** [If this hypothesis is correct, then instrumenting X will show Y]
**Test:** [Exact diagnostics to add — trace!, warn!, info_span!, event!]
**Decision:** Pending
```

Get user confirmation before instrumenting. Keep hypotheses specific and falsifiable.
Update **Decision** to `Validated` or `Invalidated [reason]` after testing.

### Phase 3: Instrument

Add **minimal, targeted diagnostics** to validate or invalidate the hypothesis:

- `trace!("description: {:?}", variable)` — for flow/data inspection
- `warn!("unexpected state: {:?}", value)` — for conditions that shouldn't happen
- `info_span!("section_name")` — for tracy profiling / timing
- `tracing::event!(Level::INFO, ?value, "checkpoint")` — for structured events

Rules:
- Tag all diagnostic additions with `// DEBUG` comment so they're easy to find and remove
- Keep diagnostics minimal — only what's needed to test the current hypothesis
- Never add diagnostics that change behavior (no `.unwrap()` additions, no control flow changes)

### Phase 4: Test

Instruct the user how to run the application and examine output:
- `cargo server` / `cargo client` variants as appropriate (inspect `.cargo/config.toml` for variants)
- Examine logs for the diagnostic output
- Compare actual output against the prediction

### Phase 5: Evaluate

**If hypothesis INVALIDATED:**
- State what the diagnostics revealed
- State why the hypothesis was wrong
- Remove diagnostics that are no longer relevant (keep useful ones)
- Return to Phase 2 with new information

**If hypothesis VALIDATED:**
- State the confirmed root cause clearly
- Proceed to Phase 6

### Phase 6: Fix

Propose a fix using sequential IDs: **F1**, **F2**, **F3**, etc. Link to the hypothesis it addresses.

```
### F[N]: [Short descriptive title] (addresses H[N])

**Root Cause:** [Confirmed explanation]
**Fix:** [What to change and why]
**Risk:** [What else this change could affect]
**Decision:** Pending
```

Get user confirmation before implementing. Update **Decision** to `Approved` or `Rejected [reason]` after review.

Implement the fix after user approval. Then verify:
1. `cargo check-all` passes
2. Runtime test confirms the bug is resolved
3. No regressions in related behavior

If fix doesn't work, the root cause hypothesis was incomplete — return to Phase 2.

### Phase 7: Remember

1. Save a memory if the bug revealed something non-obvious:
  - A footgun in the codebase architecture
  - A subtle interaction between systems
  - A pattern that will likely cause similar bug

Only save if genuinely useful for future conversations. Don't save routine fixes.

### Phase 8: Clean Up

After fix is verified:

1. Remove all `// DEBUG` diagnostic lines
2. Remove any dead code introduced during investigation
3. Keep diagnostics that are genuinely useful for future debugging (remove the `// DEBUG` tag, make them permanent with proper log levels)
4. Verify `cargo check-all` still passes

## Anti-Patterns

- **Shotgun debugging**: Changing multiple things at once hoping one works. Change one thing, test, evaluate.
- **Hypothesis-free investigation**: Adding diagnostics without a clear prediction. Always know what you expect to see.
- **Stale diagnostics**: Leaving debug instrumentation from previous hypotheses. Clean as you go.
- **Scope creep**: Fixing other issues discovered during debugging. Note them, stay focused on the reported bug.
- **Premature fixing**: Proposing a fix before confirming root cause. Instrument first.
- **Not checking for outdated dependencies or architecture**: When debugging a dependency, verify the actual version in use of a dependency. Architecture can change between major versions (e.g., lightyear 0.25 merged Predicted/Confirmed into one entity) **as well as through undocumented changes**. Check `Cargo.toml`, `git log`, and release notes and review the relevant code **before** building hypotheses.
