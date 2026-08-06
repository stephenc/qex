# Skills

A skill is a page of instructions that an agent loads when a task matches it.
The skill in `qex/` teaches an agent to use qex instead of a background command
and a polling loop.

## Install it

For one user, in every project:

```sh
mkdir -p ~/.claude/skills
cp -r skills/qex ~/.claude/skills/qex
```

For one project, so that each agent in it gets the skill:

```sh
mkdir -p .claude/skills
cp -r /path/to/qex/skills/qex .claude/skills/qex
```

Or take it from the release, with no copy of the source:

```sh
mkdir -p ~/.claude/skills/qex
curl -fsSL https://raw.githubusercontent.com/stephenc/qex/main/skills/qex/SKILL.md \
  -o ~/.claude/skills/qex/SKILL.md
```

This repository holds `.claude/skills/qex` as a link to `skills/qex`, so an
agent that works on qex itself gets the same page.

## The form

`SKILL.md` opens with a name and a description. The DESCRIPTION is the part that
decides whether an agent loads the skill, so it says WHEN to use qex and not
what qex is.

Other agent tools read a different file. The page is ordinary Markdown, so it
can be copied into `AGENTS.md`, a rule file, or a prompt with no change.

`qex help agents` holds the same material inside the binary, for an agent with
no network.
