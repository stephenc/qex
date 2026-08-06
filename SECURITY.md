# Security

## To report a fault

Use the [security
advisories](https://github.com/stephenc/qex/security/advisories/new) of this
repository. Do not open a public issue.

## The versions that get a correction

qex is at 0.x. The correction goes into the next release, and there is no
correction for an earlier version. Take the latest release.

## What you must know before you use qex

**`qex submit` copies your environment to the disk.** The file `spec.json` holds
it, with the mode 0600 in a directory with the mode 0700. If your shell holds a
token, that token goes to the file. Use `--env-capture minimal` or
`--no-env-capture` in a shell that holds secrets.

**The command line is not a secret.** `qex list` and `qex status` show the
program, its arguments and its directory. Put a secret in the environment or in
a file, and never in an argument.

**The accounting between users is cooperative.** Each coordinator writes its
claims to `/tmp/qex`, and it believes what the other users write there. The
threat model is a colleague or an agent that does not know about your work, and
it is not a person who wants to damage your work. Turn the method off with
`[peers] enabled = false`.

The [security page](https://stephenc.github.io/qex/security) gives the full
detail, including the tests that qex makes on `/tmp/qex` and the way that qex
refuses an option that the coordinator cannot obey.
