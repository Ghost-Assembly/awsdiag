# Security policy

## Reporting a vulnerability

Please report security issues privately through GitHub's
[private vulnerability reporting](https://github.com/Ghost-Assembly/awsdiag/security/advisories/new)
rather than opening a public issue.

Include what you did, what happened, and what you expected. A proof of concept
helps but is not required. Expect an acknowledgement within a week.

## What this tool touches

`awsdiag` reads AWS diagnostic data and renders it into an HTML report. Three
areas carry most of the risk, and reports about them are especially welcome.

### Credentials

The tool resolves credentials through the AWS SDK's own provider chain. It
never prompts for a key and never stores a long-lived one.

It **does** cache resolved, short-lived STS credentials on disk at
`$XDG_STATE_HOME/awsdiag/creds` (default `~/.local/state/awsdiag/creds`),
directory `0700`, files `0600` — the same class of material, with the same
protection, that the AWS CLI already caches in `~/.aws/cli/cache/`. Entries
without an expiry are never cached, and an entry is refused within 120 s of
expiring. `AWSDIAG_NO_CACHE=1` disables the cache entirely.

The cache key covers the config file path, the profile name, **and the
profile's own section** — so re-pointing a profile at a different role
invalidates the entry rather than silently reusing the previous principal's
credentials.

### Generated reports

A report embeds raw log lines, which are written by whatever produced the
logs, and reports are meant to be shared. `<`, `>`, `&`, U+2028 and U+2029 are
escaped in the embedded JSON so a log line containing `</script>` cannot close
the element, and the page builds every node with `textContent` — nothing uses
`innerHTML`. Both properties are asserted by unit tests and by a browser test.

**A report is as sensitive as the account it describes.** It contains account
identifiers, resource identifiers and raw log content. Treat one like a
production log export: do not attach it to a public issue.

### Permissions

The tool is read-only. It calls seven API operations, all `Describe`, `Get`,
`List` or `Filter`; no mutating operation exists in the codebase. See the
README for the exact list.

## Supported versions

This is pre-1.0. Fixes land on `main`; there are no maintained release
branches.
