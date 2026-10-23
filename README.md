# Outer

**A personal software forge for AI agents.** Ask your agent for an app, and get a live link you can share with friends and family.

Your agent connects to Outer over [MCP](https://modelcontextprotocol.io). It writes the app with [Ilha](https://github.com/ilhajs/ilha) + Void and commits it, and Outer builds and serves it on [celld](https://celld.dev) at `https://<slug>.outer.now` within seconds.

> [!NOTE]
> Outer is an early prototype under active development. Nothing here is ready to use yet.

## Development

You need Rust (stable) and `git`.

```bash
cargo test
OUTER_STORE=memory cargo run -- serve
curl http://127.0.0.1:8080/ready
```

`OUTER_STORE=memory` keeps everything in process, which is fine for local development. Production uses a Cloudflare R2 bucket (`OUTER_R2_ENDPOINT`, `OUTER_R2_BUCKET`, `R2_ACCESS_KEY_ID`, `R2_SECRET_ACCESS_KEY`). `outer check-config` prints every configuration problem at once.

## License

[Apache-2.0](LICENSE). Parts of Outer are derived from [Noite](https://github.com/ryuzcorp/noite), also Apache-2.0; see [NOTICE](NOTICE).
