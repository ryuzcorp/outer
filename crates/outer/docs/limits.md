# Limits

Every number is enforced in code. Usage resets daily (code changes, build
minutes) or monthly (requests, storage, egress) at UTC midnight; `whoami`
shows your current usage and when it resets.

| Limit | Free | Pro ($8/mo) |
| --- | --- | --- |
| Apps owned | 1 | 10 |
| Code changes / day | 50 | 500 |
| Build time / day | 60 min | 600 min |
| Concurrent builds / account | 1 | 2 |
| Fleet memory | 512 MiB | 1 GiB |
| Fleet CPU | 0.5 | 1.0 |
| Sleep after idle | 30 min | 24 h |
| Requests / month | 250k | 5M |
| App storage | 500 MB | 25 GB |
| Bindings | D1, KV, Durable Objects | + R2, Queues, Workflows |
| Cron minimum interval | 1 h | 1 min |
| Outbound traffic / month | 5 GB | 100 GB |
| Logs and metrics retention | 7 d | 30 d |
| API keys | 5 | 20 |

When a limit is hit the tool returns `quota_exceeded` with a `user_message`
— relay it to your human verbatim. Change, build, app and collaborator
limits are hard refusals (HTTP 200 + `isError`); requests, storage and
egress on Free pause the app at 100 % until the month resets or you
upgrade. `create_app`'s template commit and `rollback` never count as code
changes.
