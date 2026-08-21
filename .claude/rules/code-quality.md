```markdown
# Code Quality Rules

Write code that is slim, clear, readable, and maintainable.

Code should be optimized for understanding, correctness, reuse, and simplicity. Do not compress code merely to reduce the number of lines.

## Core principles

- Prefer the simplest correct solution.
- Keep code concise without sacrificing readability.
- Remove unnecessary complexity, duplication, indirection, and ceremony.
- Use existing language features, standard library functions, and project utilities when they express the intent clearly.
- Prefer clear structure over clever syntax.
- Do not introduce abstractions unless they simplify the code or support meaningful reuse.
- Do not preserve redundant code merely because it already exists.
- When a clearer or more elegant implementation is available, use it.

## Readability

- Code should be understandable without mentally decoding it.
- Use descriptive names that reflect purpose rather than implementation details.
- Keep functions focused on one coherent responsibility.
- Keep control flow direct and easy to follow.
- Prefer early returns when they reduce nesting.
- Avoid deeply nested conditionals, loops, and callbacks.
- Break complex expressions into named intermediate values when this improves clarity.
- Do not combine unrelated operations into a single statement.
- Avoid compressed one-line implementations when a slightly longer version is easier to read.
- Keep related logic together and separate unrelated concerns.

## Reuse and duplication

- Do not duplicate logic that can reasonably be shared.
- When the same operation, rule, transformation, or decision appears more than once, consider extracting it into a reusable function.
- Reuse the same function unmodified when the behavior is identical.
- Do not create several slightly different versions of the same function without a clear reason.
- Prefer one general, well-named implementation over multiple copied implementations.
- Centralize shared constants, validation rules, configuration, mappings, and calculations.
- Before adding a new helper, check whether an existing function already performs the same task.
- Do not extract trivial code merely to eliminate a few repeated lines when the extraction makes the code harder to understand.

## Functions and abstractions

- Functions should have a clear purpose and a predictable result.
- Keep function interfaces small.
- Avoid unnecessary parameters.
- Avoid boolean parameters when they create several unrelated behaviors inside one function.
- Prefer explicit functions with meaningful names over generic functions controlled by many options.
- Extract repeated behavior, not merely repeated syntax.
- Use abstraction to reveal the underlying principle of the code.
- Avoid wrapper functions that add no meaningful behavior or clarity.
- Avoid abstraction layers that merely forward values unchanged.
- Do not create classes when a function or simple data structure is sufficient.
- Prefer composition over large objects with many responsibilities.

## Data flow

- Make data flow visible and predictable.
- Prefer immutable values unless mutation clearly simplifies the implementation.
- Avoid hidden state and unexpected side effects.
- Keep state local when possible.
- Do not pass large objects when a function needs only a small part of them.
- Normalize data once at a clear boundary instead of repeatedly checking or converting it throughout the code.
- Validate inputs at appropriate boundaries.
- Represent the same concept consistently across the codebase.

## Error handling

- Handle errors where meaningful recovery, context, or translation is possible.
- Do not silently ignore errors.
- Do not catch errors only to throw them again unchanged.
- Provide error messages that explain what failed and include useful context.
- Avoid using exceptions for ordinary control flow.
- Keep failure behavior predictable.
- Remove fallback behavior that hides defects unless the fallback is an intentional product requirement.

## Comments and documentation

- Prefer self-explanatory code over comments that restate the code.
- Use comments to explain intent, constraints, tradeoffs, or non-obvious reasons.
- Do not comment obvious operations.
- Remove outdated comments when changing the related code.
- Document public functions, important assumptions, and behavior that is not clear from the interface.
- Keep documentation synchronized with the implementation.

## Performance and optimization

- Avoid unnecessary work, repeated calculations, repeated parsing, repeated queries, and repeated allocations.
- Reuse computed results when the value is unchanged.
- Choose data structures appropriate to the operations being performed.
- Avoid performance optimizations that make the code substantially harder to understand without a demonstrated need.
- Prefer algorithmic improvements over small syntax-level optimizations.
- Preserve readability unless performance requirements justify additional complexity.
- When introducing a less obvious optimization, explain why it is necessary.

## Dependencies

- Do not add a dependency for functionality that can be implemented clearly and reliably with existing tools.
- Reuse existing project dependencies when appropriate.
- Avoid overlapping dependencies that solve the same problem.
- Prefer stable and narrowly scoped dependencies.
- Remove unused dependencies, imports, variables, functions, and configuration.

## Refactoring existing code

When modifying existing code:

- Improve nearby code when the improvement is directly related and low risk.
- Remove duplication introduced or exposed by the change.
- Remove dead code made obsolete by the change.
- Reuse existing project patterns where they remain appropriate.
- Do not preserve a weak pattern merely for local consistency when it can be safely improved.
- Avoid unrelated large-scale refactoring unless it is required for a clean implementation.
- Keep behavior unchanged unless the requested change requires otherwise.
- Do not rewrite working code purely to make it look different.

## Tests

- Test behavior rather than implementation details.
- Reuse test helpers for repeated setup and assertions.
- Keep tests clear enough to describe the intended behavior.
- Avoid excessive mocking when real components or small fakes are clearer.
- Include tests for important edge cases, failure cases, and boundaries.
- Do not duplicate the same test logic across many test cases when it can be parameterized clearly.
- Keep test data minimal and relevant.

## Linters

Run the linter before calling a change done, and treat its output as part of the
build rather than advice:

- Rust: `cargo clippy --all-targets -- -D warnings`, on every crate, with the
  rustup toolchain (clippy ships there). The gate runs it as its own component
  and CI runs the same command.
- Python: `make lint` (ruff + mypy).

A lint you decide not to follow is a decision to record, not one to leave
failing: allow it at the narrowest scope that works, with a comment saying why.
This project allows `clippy::collapsible_if` crate-wide in the two GUI crates,
because the nested form keeps a comment attached to the condition it explains,
and enforces everything else.

Do not reformat code to satisfy a formatter. All three Rust crates are
hand-formatted and rustfmt disagrees with them in dozens of places; that is a
style the project has chosen, so there is no `cargo fmt --check` anywhere.

The reason this is a rule: a clippy step sat in this repo's CI for two months
without ever executing, because the repo had no remote. When it finally ran it
found an `#[allow(clippy::too_many_arguments)]` that had drifted off the function
it was written for (a doc comment and a block of constants had been inserted
between them, so it was silently allowing a `const`), and a doc comment left
behind by a function deleted in v0.5.0. Two human code reviews had passed over
both. A check that exists is not a check that runs.

## Before completing a change

Review the result and check:

- Can any code be removed without changing behavior?
- Is any logic duplicated?
- Can an existing function or principle be reused?
- Is there a clearer standard language feature available?
- Is the control flow more complicated than necessary?
- Are names precise and consistent?
- Are functions doing more than one coherent job?
- Is any abstraction unnecessary?
- Is any important behavior hidden?
- Would a new reader understand the implementation without additional explanation?
- Is the code concise because it is well designed, rather than compressed?

Deliver the cleanest implementation that remains explicit, readable, and easy to maintain.
```

