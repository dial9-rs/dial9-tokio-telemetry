# Task System Guide

## Overview

The `tasks/` directory drives an implementation agent through a feature using BDD-style end-to-end tests. Each design or bug gets its own subdirectory under `tasks/`. Task 01 establishes failing tests in a given/when/then style; each subsequent task makes one or more tests pass.

## Directory Layout

```
tasks/
├── README.md                          ← this guide
├── some-feature/                      ← subdirectory per design/bug
│   ├── 01-e2e-tests.md
│   ├── 02-some-feature.md
│   └── completed/                     ← agent moves tasks here when done
│       └── 01-e2e-tests.md
└── fix-some-bug/
    ├── 01-bug-repro-tests.md
    └── completed/
```

## Writing Tasks

### Task 01: End-to-End Tests

The first task always creates a test file with all the E2E scenarios for the feature. Each test follows given/when/then style.

Task 01 should include:
- An `Agent` field specifying which implementation agent handles this task (e.g., `implementer`)
- A reference to the design document being implemented (e.g., `Design: docs/design/feature-name.md`)
- An objective stating the feature under test
- The test file path to create
- All test cases derived from the design

Acceptance criteria for task 01:
- All tests compile
- All tests fail with clear, useful error messages indicating what's missing
- No test should panic or produce cryptic failures

### Subsequent Tasks (02+)

Each task targets one or more specific tests from task 01 and implements just enough to make them pass. A task file should include:

1. **Agent** — the implementation agent to dispatch this task to (e.g., `implementer`)
2. **Design** — reference to the design document (e.g., `docs/design/feature-name.md`)
3. **Objective** — one sentence describing what this task accomplishes
4. **Target Tests** — which E2E tests from task 01 this task makes pass
5. **Implementation** — guidance on where and how to make the change
6. **Acceptance Criteria** — the target tests pass, existing tests still pass, no warnings

### Task Dependencies

Document a dependency graph in each task set's README section (or inline in task files) when tasks have ordering constraints. Tasks that are independent of each other can be completed in any order.

## Running

Instruct the agent to pick the next task from a specific subdirectory:

```
Pick the next task from tasks/some-feature/ and implement it.
```

The implementation agent claims the next task, reads its `Agent` field, implements it following project conventions, then handles cleanup (moving the task to `completed/`) and committing the results.

## Conventions

- Each design or bug gets its own subdirectory under `tasks/` (e.g., `tasks/some-feature/`, `tasks/fix-some-bug/`).
- Task filenames: `NN-short-description.md`, zero-padded, sorted by suggested execution order.
- The agent moves completed tasks to `completed/` within the same subdirectory and does not delete them.
- The E2E test file name should appear in task 01 so subsequent tasks can reference it.
- Keep tasks small — each should be completable in a single agent iteration.

## Conflict Escalation

When an implementation agent discovers that a task
cannot succeed due to a conflict between the task's
requirements and the codebase state, it writes a
conflict file:

    tasks/<subdir>/<task-name>.conflict.md

This stops the work loop and prints a summary. To
resolve:

1. Read the conflict file to understand the issue.
2. Run `rring iter <design-name> "<feedback>"` to
   provide guidance.
3. Optionally re-run `rring design` and `rring task`
   to regenerate tasks with the new guidance.
4. Re-run `rring work` to resume.
