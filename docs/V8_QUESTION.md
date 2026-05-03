# The V8 question in Aster v0.2

## Question

v0.1 modeled read-trap continuations with a Rust enum. The load-bearing question was whether a real V8 isolate can suspend a tenant function on a missing read, let the host hydrate a capsule, and resume the same function.

## Answer

Yes, with an important narrowing: **Aster continuations are Promise continuations, not arbitrary captured V8 stacks.**

The v0.2 crate `aster-v8cell` implements the proof. It installs one host API into a V8 context:

```js
Aster.read(key, field)
```

If the key/field is present in the capsule, the callback returns a JS value. If it is missing, the callback creates a `v8::PromiseResolver`, stores the resolver in Rust with a typed trap `{ key, field }`, returns the pending Promise to JS, and expects tenant code to `await` it. The Rust scheduler then:

1. drains V8 microtasks with `perform_microtask_checkpoint`,
2. observes that the top-level Promise is pending,
3. pops the typed read trap,
4. reads the missing document from `MvccStore` at the original snapshot timestamp,
5. extends the capsule,
6. resolves the exact stored `PromiseResolver`, and
7. calls another microtask checkpoint so V8 resumes the async function.

The passing test is `aster-v8cell::tests::v8_async_function_resumes_after_read_trap` and is also covered by `crates/host/tests/crypto_and_v8.rs`:

```js
async function main() {
  const a = await Aster.read("counters/a", "value");
  const b = await Aster.read("counters/b", "value");
  return a + b;
}
```

With `counters/a` prewarmed and `counters/b` absent from the initial capsule, the function returns `42` after exactly one read trap inside a real V8 isolate.

## Why this is compatible with V8 internals

The relevant V8 APIs are explicit:

- `v8::Promise::Resolver::New(context)` creates a resolver and pending promise.
- `Resolver::GetPromise()` returns the promise passed to JS.
- `Resolver::Resolve(context, value)` resolves it later.
- `Promise::State()` and `Promise::Result()` let the embedder inspect completion.
- `Isolate::PerformMicrotaskCheckpoint()` runs queued Promise jobs.

The C++ headers shipped inside the Rust `v8` crate document these APIs in `v8-promise.h` and `v8-isolate.h`. V8 does not expose a supported API for serializing an arbitrary running JS/native stack and resuming it later. But async/await already compiles to promise continuations managed by V8. Therefore, Aster's viable continuation boundary is the same boundary Convex and Deno already use for host operations: async syscalls.

## Convex-specific evidence

The cloned `convex-backend` repository reinforces this direction. In `crates/isolate/src/environment/udf/mod.rs`, Convex invokes the user UDF, obtains a V8 `Promise`, repeatedly calls `perform_microtask_checkpoint`, pops `pending_syscalls`, runs database syscall batches, resolves stored `PromiseResolver`s, and finally reads the fulfilled promise result. In `crates/isolate/src/environment/udf/async_syscall.rs`, database reads such as `1.0/get` and `1.0/queryStreamNext` are batched and executed outside JS.

That is the production-shaped seam for Aster: replace or wrap the syscall executor for reads so missing capsule entries become typed traps to a broker, while warm entries resolve from the capsule immediately.

## What v0.2 does not prove

- It does not execute the real Convex `convex/server` module loader.
- It does not speak the Funrun remote-runner wire format.
- It does not prove that every Convex database syscall can be represented without a backend patch.
- It does not support synchronous stack capture; if a future Convex API required synchronous database reads, Aster would need deterministic replay or an upstream async boundary.

## Validation command

```bash
cargo test -p aster-v8cell
```

The full workspace test suite also runs the V8 e2e test through the host crate.
