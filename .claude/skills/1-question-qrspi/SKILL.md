---
description: Decompose a task into neutral research questions
argument-hint: "<ticket file, issue URL, or task description>"
model: openai/o4-mini-deep-research
fallbackModels: anthropic/claude-sonnet-4.6, openai/o3-deep-research, moonshotai/kimi-k2.6, xai/grok-4.1-fast-non-reasoning
thinking: medium
---

# Question -- Decompose the Task

Transform a task description into around 3-15 specific, neutral research questions. These questions drive the next phase (Research) which runs in a **separate context with no knowledge of what is being built**.

## Input

The user provides a task description, ticket file path, or issue reference.

## Process

1. **Read any provided files fully** before doing anything else.

2. **Light codebase and web exploration**:
   - Spawn a **codebase-locator** agent to find which areas of the codebase relate to the task. You need to know what exists to write good questions.
   - Spawn **web-search-researcher** agent to better understand any concepts in the task so you can include questions that lead to more insightful research. Examples: possible packages, patterns, best practices, prior art, and/or expert experience. You need to know what is possible to write good questions.

  - high-level architecture information you want to ask useful, relevant questions about how the task will affect architecture/system from a high level

3. **Decompose into around 3-15 research questions**:
   - Each question should cause a researcher to explore a different relevant area of the codebase
   - Questions must be **neutral** — they ask what exists and how it works, never how to build something
   - Prefer "trace the flow" questions that reveal architecture over yes/no questions

   Good: "How does the middleware chain handle request authentication, and where are auth policies defined?"
   Bad: "What's the best way to add a new authenticated endpoint?"

   Good: "What patterns exist for database migrations, and how are they tested?"
   Bad: "How should we add a new migration for the users table?"

4. **Determine the artifact directory**:
   - With ticket number: `doc/tasks/PROJ-1234-brief-description/` (use the project's ticket prefix)
   - Without ticket: `doc/tasks/YYYY-MM-DD-brief-description/`

5. **Create the artifact directory** if it doesn't exist (e.g., `mkdir -p doc/tasks/<id>/`).

6. **Write `task.md`** — a clean 2-3 sentence description of what's being built and why. This file persists the task context for later phases so the user doesn't have to re-explain it.

7. **Write `questions.md`** to the artifact directory:

   ```markdown
   # Research Questions

   ## Context
   [2-3 sentences describing which areas of the codebase to focus on.
   Do NOT mention what is being built or why.]

   ## Questions
   1. [Neutral, fact-seeking question]
   2. [Neutral, fact-seeking question]
   ...
   ```

8. **Present questions to the user** and wait for approval or edits before finalizing.
  - While reviewing questions with user, try to rephrase questions with additional details rather than add more, similar questions

## Output

- Directory created: `doc/tasks/<id>/`
- Files written: `doc/tasks/<id>/task.md` and `doc/tasks/<id>/questions.md`
- Tell the user: "Next: run `/qrspi:2_research doc/tasks/<id>/`"

## Rules

- `questions.md` must NOT contain the task description, goals, or desired behavior
- `task.md` is a brief, honest description of the goal — it will be read by later phases but NOT by Research
- The researcher who reads these questions should have no idea what feature is being built
- Each question should target a different area or concern
- If the task is too simple for 3 questions, tell the user — QRSPI is for complex tasks
- Try to keep the question count low by rephrasing questions with added details instead of tacking on additional questions
- Aim for 3-15 questions but 15 is not a hard-cap
