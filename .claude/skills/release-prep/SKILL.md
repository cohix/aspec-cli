---
name: release-prep
description: Prepare awman for a new release. Updates version numbers, docs, getting-started, release notes, and blog post. Does NOT run the release script.
---

# awman Release Prep Skill

Use this skill when the user asks to "prepare for release vX.Y.Z", "bump the version", or "get ready to release".

## What this skill does

1. Determine the new version from the user's request (e.g. `v0.3.0`)
2. Identify what changed since the last release (git log, new work items, new features)
3. Update all version numbers
4. Update documentation to reflect new features
5. Write release notes
6. Write a blog post

Do **not** run `make release` or push any tags. Stop before the release script.

---

## Step 1: Gather context

Run these in parallel:

```bash
# What changed since the last tag?
git log $(git describe --tags --abbrev=0)..HEAD --oneline

# What is the current version?
grep '^version' Cargo.toml

# What work items were implemented? (high-numbered ones are recent)
ls aspec/work-items/ | sort -r | head -10
```

Also read:
- `docs/releases/` — understand the format of prior release notes
- `docs/blog/` — understand the blog post style (first-person, problem-driven, no buzzwords)
- `README.md` — check if new commands or features need to be added
- `docs/00-getting-started.md` — check if new workflow steps need to be covered
- the relevant numbered doc under `docs/` (e.g. `docs/07-configuration.md`) — verify new subcommands/config fields are already documented (usually they are; added alongside the code)

---

## Step 2: Bump the version

**`Cargo.toml`** — the only place the version number lives:

```toml
version = "X.Y.Z"   # was the old version
```

Update to the new version. There is no `Cargo.lock` to worry about in this project (binary crate, not published to crates.io).

---

## Step 3: Update README.md

Check for any new top-level commands or major features that aren't mentioned. The README has a "Commands" section — add new subcommands there. Keep it brief: one line per command.

If the feature is significant enough to have its own section (like `awman claws` got in v0.2), add one. Otherwise, a line in the commands table is enough.

---

## Step 4: Update docs/00-getting-started.md

The getting-started guide covers the first-time user workflow. Add any new commands that a new user would encounter in their first session. Skip internal implementation details; focus on workflow.

Common additions:
- New subcommands with a brief example
- New flags on existing commands that change the workflow
- New monitoring or status tools

---

## Step 5: Verify the numbered docs under docs/

The numbered guides (`docs/01-concepts.md` through `docs/15-parallel-workflows.md`) are usually kept up to date alongside the code. Verify that new subcommands or config fields are documented in the relevant one. If anything is missing, add it following the existing pattern:

```markdown
### `awman <command> [flags]`

Brief description of what it does.

**Flags**

| Flag | Values | Default |
...

**Examples**

```sh
awman <command>
```
```

---

## Step 6: Write release notes

Create `docs/releases/vX.Y.Z.md`:

```markdown
# Release vX.Y.Z

## Features

- **Feature name**: one or two sentences explaining what it does and why it matters.
  - Sub-bullet for sub-commands or options if relevant.

## Improvements

- **Improvement name**: what changed and why it's better.

## Fixes

- Fixed <symptom> in <context>.
```

Keep it factual and brief. No marketing language. Focus on what a developer using awman would care about.

---

## Step 7: Write a blog post

Create `docs/blog/NNNN-slug.md` where NNNN is the next number after the last post.

**Style guide** (derived from existing posts):
- First-person narrative ("I built this...", "I've been wanting...")
- Open with the problem or itch, not the solution
- Explain *why* the feature matters before explaining *what* it does
- Focus on the improved workflows, problems solved, and benefits to security that the tool brings rather than how it works internally
- Show examples with code/shell blocks (or screenshot placeholders for the human to fill in later, don't try to do ASCII art)
- No buzzwords ("revolutionary", "game-changing", "seamless", "robust")
- No fluff ("In this post I will...", "I'm excited to announce...")
- Inlcude a quick blurb on how to install the tool in the first 1/3 of the post (the curl|sh version)
- End with a pointer to the GitHub repo and prettysmart.dev website, not a long call-to-action paragraph. Mention that feedback, issues, contributions, etc are welcome.
- Length: 400–600 words. If it's longer, you're over-explaining.

Header format:
```markdown
# awman X.Y: Short title

<short intro>

---

<quick install>

---

<main body>

---

<short outro and links>

```

---

## Checklist

Before finishing, verify:

- [ ] `Cargo.toml` version updated
- [ ] `README.md` reflects new commands/features
- [ ] `docs/00-getting-started.md` covers new workflow steps
- [ ] relevant numbered doc(s) under `docs/` document new subcommands/config fields (check, don't assume)
- [ ] `docs/releases/vX.Y.Z.md` created
- [ ] `docs/blog/NNNN-slug.md` created
- [ ] Release script NOT run (user does that separately)
