# agixd

The daemon of [Agix](https://github.com/harnixai/Agix), an agent engine: it
runs agent sessions and serves them over JSON-RPC 2.0 on a Unix socket, TCP or
WebSocket, for hosts in any language.

```sh
cargo install agixd
export ANTHROPIC_AUTH_TOKEN=sk-...   # or ANTHROPIC_API_KEY; ANTHROPIC_BASE_URL for a compatible endpoint
agixd                                # listens on ~/.agix/scenes/code/daemon.sock
```

- Running it, flags and build tiers: the
  [README](https://github.com/harnixai/Agix#running-agixd)
- The protocol, every method and event:
  [`docs/daemon_rpc_protocol.md`](https://github.com/harnixai/Agix/blob/main/docs/daemon_rpc_protocol.md)
- Embedding the engine in a Rust process instead: [`agix`](https://crates.io/crates/agix)

Licensed under Apache-2.0.
