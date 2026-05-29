---
description: Design discussion — align on where we are going before planning how
model: opus
argument-hint: "doc/tasks/<id>/"
---

# Design — Where Are We Going?

Create a design document that captures the current state, desired end state, design decisions, and patterns to follow. This is the **lowest-cost point for direction changes** — get alignment here before investing in detailed planning. You are here to gather and propose solutions on which the human considers and possibly choose.

## Input

Read `$ARGUMENTS/task.md`, `$ARGUMENTS/questions.md`, and `$ARGUMENTS/research.md`.

## Process

1. **Read all three artifacts fully.** `task.md` tells you what we're building. `research.md` tells you what exists. Ultrathink about what form the solution should take, given the research, before proceeding.

2. **Present areas needing further research, if any, and wait for answers.** Before writing anything, attempt to identify information (about code, concepts, or otherwise) that the research does not adequately cover:
   - Attempt to identify areas from which planning would benefit
   - If any identified: Present research questions with reasoning why planning would benefit from the research
   - Wait for the user to respond

   Example:
   ```
   Before I write the design document, The design would benefit from having these research questions answered:

   **Q1: [Neutral, fact-seeking question]**
   **Why: [Why planning would benefit from researching this question]**

   **Q2: ...**
   ```

   If anything is identified: Do NOT write the design document without user input.

3. **Present open questions and wait for answers.** Before writing anything, you MUST:
   - Identify and isolate the key conceptual design decisions that must be made by the human
   - Aim for ~2-5 decisions, but it depends on the scope
   - Present options with trade-offs for each, grounded in what the research found
   - Wait for the user to respond

   Example:
   ```
   Before I write the design document, I need your input on the conceptual design:
   These decisions define what we are building — including boundaries, interfaces, and major components. I will present options, but you choose the direction.
   **Q1: Data model approach**
   The research shows two patterns:
   - Option A: ...
   - Option B: ...

   Trade-offs:
   - A: ...
   - B: ...
   Which direction should we take?

   **Q2: System boundary definition**
   ...
   ```

   Do NOT skip this step. Do NOT write the design document without user input.

4. **Model the design**
   - Design is not about writing or describing code
   - Code may be referenced only as a way to illustrate or validate a concept
   - Focus on:
     - System boundaries
     - Interfaces between components
     - Responsibilities of each part
   - Avoid:
     - Function-level detail
     - Implementation-specific logic
     - Language-specific constructs unless they affect design constraints

5. **Write a concise `design.md`** to the artifact directory:

   ```markdown
   # Design Discussion

   ## Current State
   [What exists today, grounded in research findings with file:line refs]

   ## Desired End State
   [What we're building and how to verify it's correct]

   ## Patterns to Follow
   [Existing codebase patterns the implementation should match, with file:line refs.
   Flag any patterns the research found that should NOT be followed.]

   ## Design Decisions
   1. **[Decision name]**: [chosen option] — [why]
   2. **[Decision name]**: [chosen option] — [why]
   ...

   ## What We're NOT Doing
   [Explicit scope boundaries to prevent creep]

   ## Open Risks
   [Anything uncertain that might surface during implementation]
   ```

6. **Present the design to the user** for review. Iterate until they approve.

## Output

- File written: `doc/tasks/<id>/design.md`
- Tell the user: "Next: run `/qrspi:4_structure doc/tasks/<id>/`"

## Rules

- If you need deeper investigation of specific parts code for design decisions, spawn **codebase-pattern-finder** or **codebase-analyzer** agents to examine specific patterns or approaches.
- Every pattern reference must cite `file:line` from the research.
- You MUST ask questions and get approval on design designs and wait before writing. No exceptions.
- "Patterns to Follow" is critical — call out both good and bad patterns found in the codebase.
- "What We're NOT Doing" prevents scope creep downstream.
- Keep the document concise. This is a steering document, not a specification.
- When possible, present multiple candidate designs before convergence.
- Prefer comparing clearly distinct approaches rather than minor variations.

## When to Go Back

If the research is missing critical information needed for design decisions i.e. the questions missed an important area of the codebase: tell the user and suggest re-running `/qrspi:1_question` and `/qrspi:2_research` to fill the gap before proceeding with an incomplete design.
