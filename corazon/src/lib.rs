//! # corazón
//!
//! A [Cordis](https://github.com/cordiverse/cordis)-inspired plugin
//! kernel for Rust agent runtimes. *Corazón* is Spanish for heart — the kernel
//! is the heart an agent runtime beats around, and nothing more: it holds no
//! agent loop, no LLM client, no opinions about your domain.
//!
//! Everything an agent runtime is made of — LLM adapters, tools, prompt
//! sections, lifecycle hooks, domain services — is mounted by a named
//! [`Plugin`] into a shared [`Corazon`] context, and every mount is
//! **reversible**: [`Corazon::unload`] unwinds a plugin's tools, hooks, and
//! services together, returning the kernel to exactly its prior state.
//!
//! ## The four capabilities
//!
//! | Cordis concept | corazón |
//! |---|---|
//! | plugin (`inject` + `apply(ctx)`) | [`Plugin`] trait; load order resolved from `inject` |
//! | service registry (`ctx.llm`, …) | [`Corazon::provide`] / [`Corazon::service`] (typed, key-addressed) |
//! | typed events (emit / waterfall) | [`Corazon::emit`] / [`Corazon::waterfall`] |
//! | reversible effects | owner-tagged registration; [`Corazon::unload`] |
//!
//! ## Spatiotemporal composability
//!
//! Composition in **space**: which plugins are mounted, and which tools a
//! given caller sees, is decided by data ([`Corazon::load`]'s `disabled` list,
//! [`Corazon::tool_specs`]'s predicate over per-tool metadata) — not by code.
//! Composition in **time**: mounts unwind without residue, so capability can
//! come and go over a running kernel's life.
//!
//! Every registration carries its owner's name. That tag is deliberately the
//! attachment point for provenance and attestation layers built on top: a
//! plugin's mounted capabilities are enumerable claims ([`Corazon::inspect`]),
//! which a host can sign, verify, or gate.
//!
//! ## Hosting
//!
//! The kernel is generic over a [`Host`], which chooses two context types:
//! the per-call context handed to tool handlers, and the context handed to
//! event hooks. Both may borrow (they are generic associated types), so a
//! host can pass request-scoped state without cloning:
//!
//! ```
//! use corazon::{Corazon, Host, Plugin, tool_fn};
//! use serde_json::{json, Value};
//!
//! struct MyApp;
//! impl Host for MyApp {
//!     type ToolCtx<'a> = &'a str;   // e.g. your composed per-request state
//!     type HookCtx<'a> = ();
//! }
//!
//! struct Echo;
//! impl Plugin<MyApp> for Echo {
//!     fn name(&self) -> &'static str { "echo" }
//!     fn apply(&self, k: &mut Corazon<MyApp>) -> anyhow::Result<()> {
//!         k.tool(
//!             json!({}),                                   // host-defined metadata
//!             json!({"type": "function", "function": {
//!                 "name": "echo", "parameters": {"type": "object"}}}),
//!             tool_fn::<MyApp, _>(|ctx, args| Box::pin(async move {
//!                 Ok(json!({"ctx": *ctx, "args": args}))
//!             })),
//!         )?;
//!         Ok(())
//!     }
//! }
//!
//! let mut k = Corazon::<MyApp>::new();
//! k.load(vec![Box::new(Echo)], &[]).unwrap();
//! let ctx = "req-42";
//! let out = pollster::block_on(k.dispatch_tool(&ctx, "echo", &json!({"x": 1})));
//! assert_eq!(out["ctx"], "req-42");
//! k.unload("echo");
//! assert!(k.tool_specs(|_, _| true).as_array().unwrap().is_empty());
//! ```

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Chosen by the embedding application: the context types its handlers see.
///
/// Both are generic associated types so they may borrow request-scoped state;
/// the kernel itself stays `'static` and long-lived.
pub trait Host: 'static {
    /// Per-call context handed to tool handlers.
    type ToolCtx<'a>;
    /// Context handed to event hooks (often app state + an entity id).
    type HookCtx<'a>;
}

pub type ToolFn<H> = Arc<
    dyn for<'a> Fn(&'a <H as Host>::ToolCtx<'a>, &'a Value) -> BoxFuture<'a, Result<Value>>
        + Send
        + Sync,
>;
pub type HookFn<H> = Arc<
    dyn for<'a> Fn(&'a <H as Host>::HookCtx<'a>, Value) -> BoxFuture<'a, Result<Value>>
        + Send
        + Sync,
>;

/// Sugar so plugins can register plain `async fn`s:
/// `k.tool(meta, spec, tool_fn::<H, _>(|c, a| Box::pin(my_tool(c, a))))`
pub fn tool_fn<H, F>(f: F) -> ToolFn<H>
where
    H: Host,
    F: for<'a> Fn(&'a H::ToolCtx<'a>, &'a Value) -> BoxFuture<'a, Result<Value>>
        + Send
        + Sync
        + 'static,
{
    Arc::new(f)
}

pub fn hook_fn<H, F>(f: F) -> HookFn<H>
where
    H: Host,
    F: for<'a> Fn(&'a H::HookCtx<'a>, Value) -> BoxFuture<'a, Result<Value>>
        + Send
        + Sync
        + 'static,
{
    Arc::new(f)
}

/// A plugin mounts capabilities into the kernel. `inject` names the services
/// it needs; [`Corazon::load`] resolves mount order from it.
pub trait Plugin<H: Host>: Send + Sync {
    fn name(&self) -> &'static str;
    fn inject(&self) -> &'static [&'static str] {
        &[]
    }
    fn apply(&self, k: &mut Corazon<H>) -> Result<()>;
}

struct ToolEntry<H: Host> {
    owner: &'static str,
    name: String,
    /// Host-defined classification (scopes, gating flags, permissions, …).
    /// The kernel never interprets it; [`Corazon::tool_specs`] filters by it.
    meta: Value,
    spec: Value,
    f: ToolFn<H>,
}

struct HookEntry<H: Host> {
    owner: &'static str,
    f: HookFn<H>,
}

/// The shared context: service registry, tool registry, event bus — with
/// every entry tagged by the plugin that mounted it.
pub struct Corazon<H: Host> {
    // Vec keeps tool order deterministic (= registration order).
    tools: Vec<ToolEntry<H>>,
    hooks: HashMap<String, Vec<HookEntry<H>>>,
    services: HashMap<&'static str, (&'static str, Arc<dyn Any + Send + Sync>)>,
    plugins: Vec<&'static str>,
    loading: Option<&'static str>,
}

impl<H: Host> Default for Corazon<H> {
    fn default() -> Self {
        Self {
            tools: Vec::new(),
            hooks: HashMap::new(),
            services: HashMap::new(),
            plugins: Vec::new(),
            loading: None,
        }
    }
}

impl<H: Host> Corazon<H> {
    pub fn new() -> Self {
        Self::default()
    }

    fn owner(&self) -> &'static str {
        self.loading.unwrap_or("root")
    }

    // ------------------------------------------------------------ loading

    /// Mounts plugins with multi-pass dependency resolution over `inject`.
    /// Names in `disabled` are skipped — composition by configuration.
    pub fn load(&mut self, plugins: Vec<Box<dyn Plugin<H>>>, disabled: &[String]) -> Result<()> {
        let mut pending: Vec<Box<dyn Plugin<H>>> = plugins
            .into_iter()
            .filter(|p| {
                let off = disabled.iter().any(|d| d == p.name());
                if off {
                    tracing::warn!(plugin = p.name(), "disabled by config — not mounted");
                }
                !off
            })
            .collect();
        while !pending.is_empty() {
            let before = pending.len();
            let mut next = Vec::new();
            for p in pending {
                if p.inject().iter().all(|k| self.services.contains_key(k)) {
                    self.loading = Some(p.name());
                    let r = p.apply(self);
                    self.loading = None;
                    if let Err(e) = r {
                        // Reversibility holds even mid-mount: whatever this
                        // plugin managed to register before failing is
                        // unwound, so the kernel never keeps a half-plugin.
                        self.unload(p.name());
                        return Err(anyhow!("plugin {} failed to mount: {e}", p.name()));
                    }
                    self.plugins.push(p.name());
                    tracing::info!(plugin = p.name(), "mounted");
                } else {
                    next.push(p);
                }
            }
            if next.len() == before {
                let stuck: Vec<_> = next.iter().map(|p| p.name()).collect();
                return Err(anyhow!("unresolvable plugin dependencies: {stuck:?}"));
            }
            pending = next;
        }
        Ok(())
    }

    /// Reverses everything a plugin mounted — its tools, hooks, and services
    /// vanish together. The kernel returns to exactly its prior state.
    pub fn unload(&mut self, name: &str) {
        self.tools.retain(|t| t.owner != name);
        for hs in self.hooks.values_mut() {
            hs.retain(|h| h.owner != name);
        }
        self.hooks.retain(|_, hs| !hs.is_empty());
        self.services.retain(|_, (owner, _)| *owner != name);
        self.plugins.retain(|p| *p != name);
    }

    /// Plugins mounted, in mount order.
    pub fn plugins(&self) -> &[&'static str] {
        &self.plugins
    }

    // ----------------------------------------------------------- services

    /// Registers a typed service under a stable key (Cordis's `ctx.<key>`).
    pub fn provide<T: Any + Send + Sync>(&mut self, key: &'static str, svc: Arc<T>) {
        self.services.insert(key, (self.owner(), svc));
    }

    /// Resolves a service by key and concrete type.
    pub fn service<T: Any + Send + Sync>(&self, key: &str) -> Option<Arc<T>> {
        self.services
            .get(key)
            .and_then(|(_, a)| a.clone().downcast::<T>().ok())
    }

    // -------------------------------------------------------------- tools

    /// Registers a tool. `spec` is the function-calling definition handed to
    /// the model (the name is read from `spec.function.name` so spec and
    /// dispatch cannot drift apart); `meta` is host-defined classification
    /// that [`Self::tool_specs`] filters by.
    ///
    /// A duplicate name is an error: dispatch is by name, so a second
    /// registration would silently shadow the first — and the mount table
    /// ([`Self::inspect`]) would attest to a tool that never runs, which is
    /// exactly the lie a provenance layer exists to prevent.
    pub fn tool(&mut self, meta: Value, spec: Value, f: ToolFn<H>) -> Result<()> {
        let owner = self.owner();
        let name = spec["function"]["name"]
            .as_str()
            .ok_or_else(|| anyhow!("tool spec missing function.name"))?
            .to_string();
        if let Some(prior) = self.tools.iter().find(|t| t.name == name) {
            return Err(anyhow!(
                "tool {name:?} is already registered by plugin {:?}",
                prior.owner
            ));
        }
        self.tools.push(ToolEntry { owner, name, meta, spec, f });
        Ok(())
    }

    /// The tool list for one caller: entries whose `(name, meta)` pass the
    /// host's predicate, in registration order.
    pub fn tool_specs(&self, keep: impl Fn(&str, &Value) -> bool) -> Value {
        Value::Array(
            self.tools
                .iter()
                .filter(|t| keep(&t.name, &t.meta))
                .map(|t| t.spec.clone())
                .collect(),
        )
    }

    /// Dispatches one tool call. Errors become tool results, never panics —
    /// the model reads them and reacts.
    ///
    /// Dispatch is by name across EVERYTHING mounted: the kernel does not
    /// re-check `meta` here. The host's [`Self::tool_specs`] filter is the
    /// authorization boundary — whatever list the host computed for a caller
    /// is what it must permit dispatching, and nothing else.
    pub async fn dispatch_tool<'c>(
        &self,
        ctx: &'c H::ToolCtx<'c>,
        name: &str,
        args: &'c Value,
    ) -> Value {
        match self.tools.iter().find(|t| t.name == name) {
            Some(t) => (t.f)(ctx, args)
                .await
                .unwrap_or_else(|e| json!({"error": e.to_string()})),
            None => json!({"error": format!("unknown tool: {name}")}),
        }
    }

    // ------------------------------------------------------------- events

    /// Subscribes a hook to an event name.
    pub fn on(&mut self, event: &str, f: HookFn<H>) {
        let owner = self.owner();
        self.hooks
            .entry(event.to_string())
            .or_default()
            .push(HookEntry { owner, f });
    }

    /// Notify-all dispatch. Handler errors are logged, never propagated: an
    /// observer must not be able to break the flow that emitted the event.
    pub async fn emit<'c>(&self, ctx: &'c H::HookCtx<'c>, event: &str, payload: Value) {
        let Some(hs) = self.hooks.get(event) else { return };
        for h in hs {
            if let Err(e) = (h.f)(ctx, payload.clone()).await {
                tracing::warn!(event, owner = h.owner, error = %e, "hook failed");
            }
        }
    }

    /// Folds the payload through every handler in subscription order; a
    /// failing handler passes the payload through unchanged.
    pub async fn waterfall<'c>(
        &self,
        ctx: &'c H::HookCtx<'c>,
        event: &str,
        mut payload: Value,
    ) -> Value {
        let Some(hs) = self.hooks.get(event) else { return payload };
        for h in hs {
            match (h.f)(ctx, payload.clone()).await {
                Ok(v) => payload = v,
                Err(e) => tracing::warn!(event, owner = h.owner, error = %e, "hook failed"),
            }
        }
        payload
    }

    // ------------------------------------------------------ introspection

    /// The mount table: every plugin's claims, enumerable. This is the
    /// surface a provenance/attestation layer signs and verifies.
    pub fn inspect(&self) -> Value {
        json!({
            "plugins": self.plugins.iter().map(|p| json!({
                "name": p,
                "tools": self.tools.iter().filter(|t| t.owner == *p).map(|t| json!({
                    "name": t.name, "meta": t.meta,
                })).collect::<Vec<_>>(),
                "hooks": self.hooks.iter().flat_map(|(ev, hs)| {
                    hs.iter().filter(|h| h.owner == *p).map(move |_| json!(ev))
                }).collect::<Vec<_>>(),
                "services": self.services.iter()
                    .filter(|(_, (o, _))| *o == *p)
                    .map(|(k, _)| json!(k)).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "events": self.hooks.keys().collect::<Vec<_>>(),
            "toolOrder": self.tools.iter().map(|t| &t.name).collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct T;
    impl Host for T {
        type ToolCtx<'a> = ();
        type HookCtx<'a> = ();
    }

    struct Dummy;
    impl Plugin<T> for Dummy {
        fn name(&self) -> &'static str {
            "dummy"
        }
        fn apply(&self, k: &mut Corazon<T>) -> Result<()> {
            k.provide("dummy-svc", Arc::new(42_i64));
            k.tool(
                json!({"core": true}),
                json!({"type":"function","function":{"name":"noop","parameters":{}}}),
                tool_fn::<T, _>(|_c, _a| Box::pin(async { Ok(json!({"ok": true})) })),
            )?;
            k.on(
                "ping",
                hook_fn::<T, _>(|_c, mut p| {
                    Box::pin(async move {
                        p["seen"] = json!(true);
                        Ok(p)
                    })
                }),
            );
            Ok(())
        }
    }

    struct Needy;
    impl Plugin<T> for Needy {
        fn name(&self) -> &'static str {
            "needy"
        }
        fn inject(&self) -> &'static [&'static str] {
            &["dummy-svc"]
        }
        fn apply(&self, _k: &mut Corazon<T>) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn unload_unwinds_everything() {
        let mut k = Corazon::<T>::new();
        k.load(vec![Box::new(Dummy)], &[]).unwrap();
        assert!(k.service::<i64>("dummy-svc").is_some());
        assert_eq!(k.tool_specs(|_, _| true).as_array().unwrap().len(), 1);
        k.unload("dummy");
        assert!(k.service::<i64>("dummy-svc").is_none());
        assert!(k.tool_specs(|_, _| true).as_array().unwrap().is_empty());
        assert!(k.plugins().is_empty());
    }

    #[test]
    fn inject_orders_mounts() {
        // Needy listed first but depends on Dummy's service; load resolves it.
        let mut k = Corazon::<T>::new();
        k.load(vec![Box::new(Needy), Box::new(Dummy)], &[]).unwrap();
        assert_eq!(k.plugins(), &["dummy", "needy"]);
    }

    #[test]
    fn unresolvable_dependency_errors() {
        let mut k = Corazon::<T>::new();
        assert!(k.load(vec![Box::new(Needy)], &[]).is_err());
    }

    #[test]
    fn dispatch_and_events() {
        let mut k = Corazon::<T>::new();
        k.load(vec![Box::new(Dummy)], &[]).unwrap();
        let out = pollster::block_on(k.dispatch_tool(&(), "noop", &json!({})));
        assert_eq!(out["ok"], true);
        let out = pollster::block_on(k.dispatch_tool(&(), "nope", &json!({})));
        assert!(out["error"].as_str().unwrap().contains("unknown tool"));
        let w = pollster::block_on(k.waterfall(&(), "ping", json!({})));
        assert_eq!(w["seen"], true);
    }

    #[test]
    fn disabled_is_composition_by_config() {
        let mut k = Corazon::<T>::new();
        k.load(vec![Box::new(Dummy)], &["dummy".to_string()]).unwrap();
        assert!(k.plugins().is_empty());
    }

    /// A second registration under an existing tool name must refuse to
    /// mount — silent shadowing would make the mount table attest to a tool
    /// that never dispatches.
    struct Shadow;
    impl Plugin<T> for Shadow {
        fn name(&self) -> &'static str {
            "shadow"
        }
        fn apply(&self, k: &mut Corazon<T>) -> Result<()> {
            // Registers one legitimate tool, then collides with Dummy's.
            k.tool(
                json!({}),
                json!({"type":"function","function":{"name":"mine","parameters":{}}}),
                tool_fn::<T, _>(|_c, _a| Box::pin(async { Ok(json!({})) })),
            )?;
            k.tool(
                json!({}),
                json!({"type":"function","function":{"name":"noop","parameters":{}}}),
                tool_fn::<T, _>(|_c, _a| Box::pin(async { Ok(json!({})) })),
            )?;
            Ok(())
        }
    }

    #[test]
    fn duplicate_tool_names_refuse_to_mount() {
        let mut k = Corazon::<T>::new();
        let err = k.load(vec![Box::new(Dummy), Box::new(Shadow)], &[]).unwrap_err();
        assert!(err.to_string().contains("noop"), "error names the colliding tool: {err}");
    }

    /// A plugin that fails mid-apply leaves no residue: its earlier
    /// registrations unwind, and everything mounted before it survives.
    #[test]
    fn failed_mount_unwinds_partial_registrations() {
        let mut k = Corazon::<T>::new();
        assert!(k.load(vec![Box::new(Dummy), Box::new(Shadow)], &[]).is_err());
        // Dummy is intact...
        assert_eq!(k.plugins(), &["dummy"]);
        assert!(k.service::<i64>("dummy-svc").is_some());
        // ...and Shadow's successful first registration is gone.
        let names: Vec<String> = k
            .tool_specs(|_, _| true)
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["noop"], "the half-mounted plugin left residue: {names:?}");
    }
}
