# agix

The crate a Rust host depends on to embed [Agix](https://github.com/harnixai/Agix),
an agent engine: assemble a session pool, create sessions, run turns, receive
events and answer the engine's questions, or serve the engine's JSON-RPC
protocol on any byte stream in your own process (`agix::embed`).

```toml
[dependencies]
agix = "0.4"
```

Depend on `agix` alone. The `agix-*` crates it pulls in are the engine's
implementation and change without notice; everything an embedder needs is
re-exported here, by theme.

- The embedding guide, with every snippet compiled as a doctest: the `guide`
  module on [docs.rs](https://docs.rs/agix), or
  [`docs/agix-sdk.md`](https://github.com/harnixai/Agix/blob/main/docs/agix-sdk.md)
- Features (`coding`, `scripts`, `plugin-packages`, `sandbox`, `otel`, …): §1 of
  the guide
- Versions and compatibility: §9 of the guide
- The daemon built on this crate: [`agixd`](https://crates.io/crates/agixd)

Licensed under Apache-2.0.
