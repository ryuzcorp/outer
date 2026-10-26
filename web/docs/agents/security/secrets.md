---
title: Store API keys as secrets
description: Set, list, and unset encrypted per-app secrets outside code.
---

Keep keys outside code with three tools: `set_secret {app, key, value}`, `list_secrets {app}`, `unset_secret {app, key}`. Values rest encrypted (AES-256-GCM, one key per app).

## Follow the secret rules

| Rule | Value |
| --- | --- |
| Name shape | `^[A-Z_][A-Z0-9_]{0,63}$` |
| Reserved names | `PORT`, `AWS_*`, `OUTER_*`, `LD_*` refused |
| Value size | At most 4 KiB each |
| Count | At most 50 per app |
| Visibility | Never returned, logged, or echoed |

## Apply secrets on the next deploy

1. Deploy after setting a secret; setting never touches the running fleet.
2. Read secrets as Worker env vars (`env.MY_KEY`); each deploy decrypts them into config `vars`.
3. Name secret keys exactly; a same-named `vars` entry in code loses and the deploy log names the overwrite.
4. Keep secrets out of build scripts; `bun install` and `bun run build` never see them.

For the person-side walkthrough, see [Secrets](/security/secrets).
