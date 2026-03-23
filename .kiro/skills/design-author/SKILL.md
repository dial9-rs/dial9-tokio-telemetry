---
name: design-author
description: Create a new design document from abstract requirements. Use when the user describes a feature or change and needs a formal design doc written to docs/design/.
---

## Purpose

Transform abstract feature requirements into a structured design
document following the project's conventions in `docs/design/README.md`.

## Process

1. Read `docs/design/README.md` to understand the required structure.
2. Read existing design documents in `docs/design/` to calibrate
   the level of detail, tone, and formatting conventions used in
   this project.
3. Read relevant source files to understand the current codebase
   architecture so the design references real types, services,
   and file paths.
4. Draft the design document with all required sections:
   - **Overview**
   - **Current CX / Concepts**
   - **Proposed CX / CX Specification** — command syntax,
     validation rules, error messages, example output
   - **Technical Design / Technical Approach** — model changes,
     service methods, validation logic
   - **Code Architecture / File Changes**
   - **Testing Strategy** — unit and integration test plan
   - **Implementation Order / Phases**
   - **Open Questions** (if any)
5. Write the document to `docs/design/<feature-name>.md` using a
   kebab-case filename derived from the feature name.

## Guidelines

- The Proposed CX section is the most important — it is the
  specification that gets built. Include concrete command examples,
  all validation rules, and exact error message text.
- Technical Design should reference actual types and file paths
  from the codebase, not hypothetical ones.
- Word wrap prose at 72 columns, matching existing documents.
- Use fenced code blocks for command examples and code snippets.
- Keep the document self-contained: a developer should be able to
  implement the feature from the design doc alone.
