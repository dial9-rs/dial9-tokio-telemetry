---
name: design-audit
description: Audit and improve an existing design document. Use after a design doc has been drafted to verify technical accuracy, find gaps, and fix issues.
---

## Purpose

Review a design document for correctness, completeness, and
consistency, then apply fixes directly to the document.

## Process

### 1. Research

Before evaluating the design, build domain knowledge:

- Search the web for relevant documentation on APIs, behaviors,
  limits, and best practices that relate to the design.
- Read the project's existing design documents in `docs/design/`
  to understand established patterns and conventions.
- Read relevant source code to verify that types, file paths,
  and service methods referenced in the design actually exist
  or are plausible extensions of the current architecture.

### 2. Evaluate

Check the design for:

- **Factual accuracy**: Do behaviors described in the design
  match actual documentation and API semantics? Are limits,
  default values, and semantics correct?
- **Internal consistency**: Do the CX examples match the
  technical design? Do error messages reference the right
  names and types? Do file change lists cover all sections
  that describe modifications?
- **Completeness**: Are all validation rules specified? Are
  error messages provided for every failure case? Does the
  testing strategy cover the described behaviors?
- **Sharp edges**: Are there edge cases not addressed? Race
  conditions? Conflicting behaviors with existing features?
  Ambiguous specifications that could be interpreted multiple
  ways?
- **Underspecification**: Are there sections that say "TBD" or
  leave decisions open that should be resolved? Are default
  values specified? Are boundary conditions defined?

### 3. Fix

Apply corrections directly to the design document:

- Fix factual errors with correct information from documentation.
- Resolve inconsistencies by making the document internally
  coherent.
- Add missing validation rules, error messages, or edge case
  handling.
- Clarify ambiguous specifications with concrete decisions.
- Move genuinely open questions to the Open Questions section
  rather than leaving them inline.
- Do not remove content that is correct; only add, clarify,
  or correct.

### 4. Summary

After making changes, provide a brief summary of what was
found and fixed. Do not write a long report — just list the
key issues addressed.
