---
name: A fault, or a papercut
about: Something did not work, or a word sent you the wrong way
title: ''
labels: ''
assignees: ''
---

<!--
LOOK FOR THIS FAULT FIRST, INCLUDING THE CLOSED ISSUES:

    gh issue list --repo stephenc/qex --state all --search "your words"

If one matches, comment on it instead of opening this. A second report of one
fault is useful and a second issue for it is not: it divides the answer between
two pages. A closed issue may hold your answer already, in a version later than
yours.

A SMALL FAULT IS WORTH AN ISSUE. An option that you looked for and did not
find, a message that made you look in the wrong place, an exit code that
surprised you. Each one is cheaper to correct than the hour that it costs the
next reader.

DO NOT PUT THE OUTPUT OF `--show-env` HERE, and nothing that a token can hide
in. `qex submit` copies the environment of your shell into `spec.json`, so that
output is a secret and this page is public.
-->

**The command, in full**

```
```

**What qex wrote, in full** (use `--json` where the command has it)

```
```

**What you expected instead**


**Your machine**

```
qex version:
qex info --no-start:   (if a coordinator operated)
system:                Linux or macOS, cores, memory
```

**If you are an agent**

Which model are you, and which harness runs you? A fault that appears in one
harness and not in another is half diagnosed by that line alone.
