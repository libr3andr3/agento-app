# corazón

A [Cordis](https://github.com/cordiverse/cordis)-inspired plugin kernel for Rust agent runtimes.

*Corazón* is Spanish for heart. This crate is the heart an agent runtime beats around — and nothing more: no agent loop, no LLM client, no domain opinions. You bring those; corazón makes them **composable in space and time**.

## The idea

Everything an agent runtime is made of — LLM adapters, tools, prompt sections, lifecycle hooks, domain services — is mounted by a named **plugin** into a shared kernel, and every mount is **reversible**: `unload(plugin)` unwinds that plugin's tools, hooks, and services together, returning the kernel to exactly its prior state.

| Cordis concept | corazón |
|---|---|
| plugin (`inject` + `apply(ctx)`) | `Plugin` trait; mount order resolved from `inject` |
| service registry (`ctx.llm`, …) | `provide` / `service` — typed, key-addressed |
| typed events | `emit` (notify-all, observer errors can't break the emitter) and `waterfall` (fold a payload through handlers) |
| reversible effects | owner-tagged registration; `unload` |

**Space:** which plugins mount (`load`'s disabled list) and which tools a given caller sees (`tool_specs` filters host-defined per-tool metadata — scopes, permission tiers, per-tenant gating) is decided by data, not code.

**Time:** mounts unwind without residue over a running kernel's life.

**Provenance-ready:** every registration carries its owner. `inspect()` returns the full mount table — each plugin's enumerable claims — which is deliberately the attachment surface for signing and attestation layers built on top.

## Hosting

The kernel is generic over a `Host`, which picks the two context types handlers receive. Both are GATs, so they may borrow request-scoped state while the kernel stays long-lived:

```rust
use corazon::{Corazon, Host, Plugin, tool_fn};
use serde_json::{json, Value};

struct MyApp;
impl Host for MyApp {
    type ToolCtx<'a> = &'a str;  // your composed per-request state
    type HookCtx<'a> = ();
}

struct Echo;
impl Plugin<MyApp> for Echo {
    fn name(&self) -> &'static str { "echo" }
    fn apply(&self, k: &mut Corazon<MyApp>) -> anyhow::Result<()> {
        k.tool(
            json!({"scope": "customer"}),  // host-defined metadata
            json!({"type": "function", "function": {
                "name": "echo", "parameters": {"type": "object"}}}),
            tool_fn::<MyApp, _>(|ctx, args| Box::pin(async move {
                Ok(json!({"ctx": *ctx, "args": args}))
            })),
        )?;
        Ok(())
    }
}

let mut k = Corazon::<MyApp>::new();
k.load(vec![Box::new(Echo)], &[])?;
let specs = k.tool_specs(|_name, meta| meta["scope"] == "customer"); // → your LLM client
let out = k.dispatch_tool(&"req-42", "echo", &json!({"x": 1})).await;
k.unload("echo"); // everything Echo mounted is gone
```

A fuller walkthrough — two plugins, prompt waterfall, tool dispatch, turn events, unload — is in [`examples/receptionist.rs`](examples/receptionist.rs):

```
cargo run --example receptionist
```

## Design rules

1. **The kernel interprets nothing.** Tool `meta` is opaque host data; event payloads are JSON; specs pass through to your model verbatim.
2. **Errors are results.** A failed tool call becomes `{"error": …}` for the model to read; a failed event hook is logged and skipped — an observer must never break the flow that emitted the event.
3. **Deterministic order.** Tools list in registration order; hooks fire in subscription order; plugin mount order is resolved from `inject` and otherwise preserved.
4. **Reversibility from ownership.** No teardown closures to get wrong: unwinding is `retain(owner != plugin)` across every registry.

## Status / roadmap

Extracted from a production agent runtime (agente). v0.1 ships `emit` + `waterfall`; Cordis's `parallel` / `serial` modes, hot reload of a live plugin, and the attestation layer over `inspect()` are future work.

## License

MIT OR Apache-2.0, at your option.
