---
title: Read logs and errors after deploys
description: Use app_logs, app_errors, and deploy_log to triage failures.
---

## Pick the right log tool

| Tool | Holds | When to read |
| --- | --- | --- |
| `app_logs` | Fleet stdout/stderr, last 2000 lines. | Debug runtime behavior and server-side capture gaps. |
| `app_errors` | Fingerprint-grouped runtime errors. | Read after every deploy that touches server code. |
| `deploy_log` | Build output for one deploy. | Debug a failed build step. |

## Triage in order

1. Read `app_errors` after every deploy that touches server code.
2. Triage `console.error` output over MCP when the page reports errors.
3. Open `app_logs` for the last 2000 lines of fleet stdout/stderr.
4. Open `deploy_log` for build output when the failure sits in the pipeline.

For the person-side walkthrough, see [Read logs and errors](/testing/logs-errors).
