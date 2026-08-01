---
name: vision-alignment
description: "Enforce alignment with VISION.md when designing, planning, or modifying game systems. MUST trigger when: (1) adding or modifying gameplay features, ECS components/systems, game modes, or brawler mechanics, (2) creating implementation plans or design documents, (3) making architectural decisions about game systems, (4) adding new content types (stages, items, challenges). Does NOT trigger for: pure infrastructure work (CI, build config, dev tooling), code style fixes, or refactors that don't change game behavior."
---

# Vision Alignment

Read `VISION.md` at the project root before proceeding. All design and implementation work must align with it.

## Checklist

Before proposing or implementing changes, verify:

1. **Pillar alignment** -- Does this support one of the four design pillars (Living Home-Base, Open-World Exploration, Varied Challenges, Meaningful Progression)? If not, justify why the project needs it.
2. **System coherence** -- Does this integrate with existing brawler systems (stats, alignment, genetics, appearance)? New systems must connect to at least one existing system, not stand alone.
3. **Mode fit** -- If adding gameplay, does it fit an existing game mode or define a new one with clear rules? Avoid mechanics that don't map to any mode.
4. **World structure** -- Does this respect the Home-Base / Overworld / Instanced Stages separation? Features should know which context they belong to.
5. **Progression impact** -- Does this contribute to the long-term loop (raise, train, shape, breed, inherit)? Avoid dead-end features that don't feed progression.
6. **Vision drift** -- Does this contradict or dilute any existing vision element? Flag conflicts explicitly rather than silently overriding.

## When Conflicts Arise

If a requested change conflicts with VISION.md:

1. State the specific conflict (quote the relevant VISION.md section).
2. Ask the user whether to (a) adapt the implementation to fit the vision, or (b) propose a VISION.md amendment.
3. Do not silently implement something that contradicts the vision.

## Design Decisions

VISION.md contains explicit design decisions (lifespan model, permadeath stance, economy rules, monetization policy). Treat these as constraints, not suggestions. Implementations that violate them require explicit user approval.

## Keeping VISION.md Current

When a design decision is changed, added, or overridden with user approval, update VISION.md to reflect the new state. The vision document must remain the authoritative source of truth -- not conversation history or memory. This includes:

- Amending existing sections when decisions evolve
- Adding new game modes, systems, or mechanics that the user has approved
- Updating design decisions when the user explicitly changes course
