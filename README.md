# Content Repurposing

An AI content repurposing SaaS: one long piece in, platform-native drafts out, in the
creator's voice, with every claim traced to the source and nothing published without a
person's approval.

> **Status: base setup only.** Nothing of the product is built yet; the code is the
> renamed Backend Platform starter template. The contract is `docs/END_GOAL.md`.

## Quick start (the template, local)

Docker, rustup (the pinned toolchain installs itself), curl, jq, python3. Everything
binds 127.0.0.1 inside this product's port band.

```sh
scripts/stack.sh up        # PostgreSQL 18 in Docker, published on 127.0.0.1:63432
scripts/dev.sh start       # build, create and migrate the dev database, run the server
curl -fsS http://127.0.0.1:63450/readyz
# {"database":"ok","status":"ready"}
invite=$(scripts/dev.sh invite)
scripts/dev.sh stop
```

The accounts walk-through — sign-up, verification, a workspace, an API token — is
`docs/ACCOUNTS.md`; the operator guide is `docs/OPERATIONS.md`. `scripts/check.sh` is
the gate CI runs.

## Layout

- `crates/repurpose-{http,db,accounts,server}` — the template's crates, renamed
- `deploy/`, `scripts/`, `docs/`, `perf/` — host layer, drills, operator docs, evidence kit
- `web/` — empty until the design-system decision

Licence: MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`).
