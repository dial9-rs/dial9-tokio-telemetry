---
name: task-audit
description: Audit task files in a tasks/ subdirectory for conflicts, missing dependencies, and misalignment with the design document. Use after tasks have been authored to verify and correct them.
---

## Purpose

Review the task files in a `tasks/<name>/` subdirectory against
the source design document, identify problems, and fix them in place.

## Process

### 1. Load context

- Read `tasks/README.md` for conventions.
- Read the design document that the tasks were derived from.
- Read all task files in the specified `tasks/<name>/` subdirectory
  (excluding `completed/`).

### 2. Check alignment with design

For each behavior, validation rule, and error message in the
design document:

- Verify it appears as a test case in Task 01.
- Verify a subsequent task targets that test.
- Flag any design requirement with no corresponding task or test.

Flag any task or test that describes behavior not present in the
design document.

Verify that all task files include a reference to the design
document being implemented (e.g., `Design: docs/design/feature-name.md`).

### 3. Check dependency ordering

- Build the dependency graph from task references and implicit
  ordering (data model before service methods before CLI wiring).
- Flag any task that references types, methods, or behaviors
  introduced by a later-numbered task.
- Flag circular dependencies.

### 4. Check internal consistency

- Verify target test names in tasks 02+ match actual test names
  in Task 01.
- Verify file paths and type names referenced in tasks match the
  codebase or the design document.
- Verify acceptance criteria are concrete and testable.

### 5. Fix problems

Edit task files in place to correct:

- Missing design document references — add them to all task files.
- Missing test cases — add them to Task 01.
- Missing tasks — create new task files with appropriate numbers,
  renumbering subsequent tasks if needed.
- Dependency violations — reorder or add explicit dependency notes.
- Name mismatches — correct test names and type references.
- Orphaned tasks — remove tasks that don't map to the design.

### 6. Summary

List the issues found and fixes applied. Keep it brief.
