---
description: Invoked by user to plan implementation from rpi research
---

# Implement Plan

You are tasked with **orchestrating** the implementation of an approved technical plan from `doc/plans/`. These plans contain phases with specific changes and success criteria.

**CRITICAL: Your role is to act as a driver-orchestrator, not as the primary implementer.** Delegate implementation work to specialized subagents wherever appropriate.

## Getting Started

When given a plan path:
- Read the plan **completely** and check for any existing checkmarks (- [x])
- Read the original all files mentioned in the plan
- **Read files fully** - never use limit/offset parameters, you need complete context
- Think deeply about how the pieces fit together
- Create a todo list to track your progress
- create a TodoWrite checklist of every deliverable
- **Identify which subagents** are needed for each phase
- Delegate to subagents for implementation work

If no plan path provided, ask for one.

## Implementation Philosophy

Plans are carefully designed, but reality can be messy. Your job is to:
- **Orchestrate** the implementation by delegating to specialized subagents
- Follow the plan's intent while adapting to what you find
- Ensure each phase is fully implemented before moving to the next
- Verify work makes sense in the broader codebase context
- Update checkboxes in the plan as you complete sections

When things don't match the plan exactly, think about why and communicate clearly. The plan is your guide, but your judgment matters too.

## Leveraging Subagents

**You should delegate implementation work to specialized subagents instead of doing it yourself.** Use the Task tool with appropriate `subagent_type` to spawn experts for each implementation phase.

### When to Use Subagents

**Always use subagents for:**
- **Code implementation** - Any actual code writing, editing, or refactoring
- **Testing** - Writing tests, test automation, debugging complex issues
- **Architecture decisions** - Designing system structure, choosing patterns
- **Performance work** - Profiling, optimization, benchmarking
- **Security tasks** - Security audits, vulnerability fixes
- **Documentation** - Writing comprehensive docs, API documentation

**You should handle directly:**
- **Reading and understanding** the plan and existing code
- **Updating plan checkmarks** to track progress
- **Coordinating** between multiple subagents
- **Making high-level decisions** about which subagents to use
- **Communicating** with the user about progress

### Example Delegation Pattern

For a typical implementation phase:

1. **Analysis Phase**: You read the plan and code directly
2. **Architecture Phase**: Delegate to `architect-reviewer` or language-specific architect
3. **Implementation Phase**: Delegate to appropriate language specialist (e.g., `rust-engineer` for Rust code)
4. **Testing Phase**: Delegate to `test-automator` or `qa-expert`
5. **Review Phase**: Delegate to `code-reviewer` or `performance-engineer`
6. **Documentation Phase**: Delegate to `documentation-engineer`

### How to Delegate

When delegating to a subagent, provide:
- **Clear context** from the plan
- **Specific phase details** they need to implement
- **Files to modify** or create
- **Success criteria** from the plan
- **Request for report** on what was done and any deviations

After a subagent completes work:
- Review their output
- Update the plan checkmarks
- Verify success criteria
- Decide on next phase or subagent

If you encounter a mismatch:
- STOP and think deeply about why the plan can't be followed
- Present the issue clearly:

  ```
  Issue in Phase [N]:
  Expected: [what the plan says]
  Found: [actual situation]
  Why this matters: [explanation]

  How should I proceed?
  ```

## Verification Approach

After implementing a phase:
- Run the success criteria checks (usually `cargo test-all` covers everything)
- Fix any issues before proceeding
- Update your progress in both the plan and your todos
- Check off completed items in the plan file itself using Edit

Don't let verification interrupt your flow - batch it at natural stopping points.

## If You Get Stuck

When something isn't working as expected:
- First, make sure you've read and understood all the relevant code
- Consider if the codebase has evolved since the plan was written
- Present the mismatch clearly and ask for guidance

Use sub-tasks sparingly - mainly for targeted debugging or exploring unfamiliar territory.

## Resuming Work

If the plan has existing checkmarks:
- Trust that completed work is done
- Pick up from the first unchecked item
- Verify previous work only if something seems off

Remember: You're implementing a solution, not just checking boxes. Keep the end goal in mind and maintain forward momentum.

## Concluding work
Before marking a phase complete, re-read the plan document and verify EVERY bullet point under this phase's success criteria. List each criterion and whether it's implemented. Do not skip any.
