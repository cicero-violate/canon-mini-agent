# Simplify Rule

Continuously replace repetition with a single source of truth.

## What This Means
- Identify duplicated logic, text, or data structures.
- Extract the shared part into a helper, constant, or utility.
- Recompose the original behavior from the shared piece.
- Repeat until duplication is eliminated without changing outcomes.

## Why It Works
- Fewer lines to maintain.
- Fewer places to update when behavior changes.
- Lower risk of drift and inconsistency.

## Concrete Refactoring Patterns
- **Extract common strings:** Move repeated text blocks into `const` strings and format them with small helpers.
- **Replace repeated branches with a table:** Map keys to functions/closures and dispatch via lookup instead of `if`/`match` duplication.
- **Use data-driven formatting:** Store bullet lists/steps in arrays and render them with a formatter instead of repeating literals.
- **Normalize shared validation:** Consolidate similar validation paths into one function that accepts a context parameter.
- **Centralize error handling:** Return structured errors from one helper and reuse it at every call site.
- **Coalesce repeated I/O paths:** Wrap file reads/writes in a helper that handles logging, errors, and truncation consistently.
- **Collapse near-identical enums:** Replace several adjacent variants with a single variant carrying a small enum or tag.
- **Inline trivial wrappers:** If a wrapper only forwards arguments, remove it and use the underlying call directly.
- **Extract repeated predicates:** Move repeated `if` conditions into `fn is_x(...)` helpers for reuse and clarity.

## Checklist
- Any block repeated 2+ times?
- Can it become a function or constant?
- Can the same formatting be produced by a formatter + data?
- Did behavior stay identical?
