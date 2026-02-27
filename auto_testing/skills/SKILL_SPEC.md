# Skill Spec (auto_testing)

## 1. Purpose

Standardize all skills under `auto_testing/skills/` so agents can parse and execute them consistently.

## 2. Required Sections

Each skill file must include:

1. `Responsibility`: What this skill owns.
2. `Trigger Conditions`: When this skill must be invoked.
3. `Inputs`: Required files, env vars, and parameters.
4. `Execution Steps`: Ordered, command-level actions.
5. `Output Artifacts`: Files that must be produced.
6. `Failure Handling`: How to classify/retry/fail.
7. `Exit Criteria`: What counts as done.

## 3. Common Rules

1. Reuse existing repo scripts/configs; do not add hidden logic.
2. Persist all outputs under `artifacts/`.
3. Persist all logs under `artifacts/logs/`.
4. Use `artifacts/coverage/` for coverage read/write.
5. If rules conflict, `auto_testing/coverage_map.yaml` is the source of truth.

## 4. Naming Rules

1. Filename format: `tipg-<skill-name>.md`.
2. One file describes one skill.
3. Use English for machine-facing content (paths/commands/field names).
