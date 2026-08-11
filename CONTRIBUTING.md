<!-- Mirrors nxm-rs/.github CONTRIBUTING.md. This repository will move to
the nxm-rs organisation, after which the org-level file applies directly
and this copy goes away. Keep them in step until then. -->

# Contributing to nxm-rs

Thanks for considering contributing. Here is how to do it without wasting time.

## Before You Start

1. **Check existing issues/PRs**: Someone might already be working on it.
2. **Open an issue first**: Unless it is a typo fix. Let us agree on the problem before you code the solution.
3. **One PR, one thing**: Do not bundle unrelated changes.

## Writing Style

All documentation, comments, and written communication must use **Oxford English**:

- British spelling with -ize endings (e.g. organize, recognize, not organise, recognise)
- British vocabulary (e.g. colour, behaviour, centre)
- No em dashes. Use colons, semicolons, or restructure the sentence.
- Contractions are acceptable in informal documentation (e.g. README files).

## AI Assistance Disclosure

**If AI wrote any part of your contribution, you must say so in the PR.**

This includes:
- Code generation beyond basic autocomplete
- Documentation or comments
- PR descriptions and commit messages
- Review responses

### Why We Care

1. **Respect for reviewers**: We need to know if we are reviewing human or AI work. It affects how we review.
2. **You own your code**: If you submit it, you must understand it. Be ready to explain every line.
3. **Quality bar**: AI often generates mediocre code that needs heavy editing. Low-quality AI slop will be rejected.

### How to Disclose

In your PR description, add:
```
AI Assistance: [Tool name] used for [what parts]
```

Example: "AI Assistance: GitHub Copilot used for test boilerplate generation"

Not disclosing AI use when discovered will result in PR closure and potentially a ban. We use AI detection tools and we are good at spotting patterns.

## Code Standards

- **It should work**: Test your code. "It compiles" is not enough.
- **It should be readable**: Code is read more than written.
- **It should be documented**: At least explain the why, if not the what.
- **It should follow conventions**: Match the existing code style.

## Pull Request Process

### Before Opening a PR

1. **Link to an issue**: PRs without linked issues may be ignored or closed. We discuss first, code second.
2. **Fork and branch**: Create your branch from `main`.
3. **Self-review**: Review your own code before wasting our time. Check:
   - All tests pass
   - No compiler warnings
   - Code follows existing patterns
   - You have actually tested it manually

### PR Requirements

Your PR description must include:
- **What**: Clear description of changes
- **Why**: Link to issue being solved
- **Testing**: How you tested it (unit tests are not enough)
- **AI Assistance**: Disclosure if AI was used

### After Opening a PR

1. Be responsive to feedback or your PR gets closed.
2. Be ready to explain your code.
3. Do not argue about style: use the formatter.

## What We Are Looking For

**We want:**
- Bug fixes for actual bugs
- Performance improvements with benchmarks
- Features that solve real problems
- Documentation that helps developers
- Tests that catch real issues

**We do not want:**
- Cosmetic changes with no real benefit
- "Refactoring" that does not improve anything measurable
- Features nobody asked for
- Breaking changes without strong justification
- PRs that do not pass CI

## Code Review

Expect honest feedback. We care about:
- Does it work?
- Is it maintainable?
- Does it make the codebase better?

We do not care about:
- Perfect variable names
- Tabs vs spaces (use the formatter)
- Your feelings

## Getting Help

- Open an issue with your question
- Check the existing documentation
- Read the code: it is often clearer than docs

## Licence

By contributing, you agree your contributions will be licensed under the same licence as the project.

---

**TL;DR**: Make it work, make it clear, make it tested. PRs welcome if they actually improve things.
