//! V8-backed read-trap continuation runtime for Aster.
//!
//! This crate answers the load-bearing v0.1 question: can a tenant JavaScript
//! function suspend on a missing capsule read, let the host hydrate data, and
//! resume inside a real V8 isolate without rebuilding V8's scheduler?
//!
//! The answer demonstrated here is "yes, if the continuation boundary is an
//! `await` over a host-created Promise". We do not attempt to capture arbitrary
//! synchronous JS stacks. The legacy host API `Aster.read(key, field)` and the
//! Convex-shaped `Convex.asyncSyscall("1.0/get", argsJson)` shim both return
//! values immediately for warm capsule entries and pending Promises for missing
//! reads. V8 preserves the async continuation. The Rust host receives a typed
//! trap, hydrates the capsule through its broker, resolves the promise, runs a
//! microtask checkpoint, and the same JS async function returns the final value.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::sync::{Mutex, Once};

use aster_broker::{BrokerError, CapsuleBrokerClient};
use aster_capsule::{
    DeploymentId, Document, DocumentId, MvccStore, SealContext, SnapshotCapsule, TenantId, Value,
};

static V8_INIT: Once = Once::new();

fn init_v8() {
    V8_INIT.call_once(|| {
        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();
    });
}

/// A typed read trap emitted by a real V8 isolate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct V8ReadTrap {
    pub key: DocumentId,
    pub field: String,
}

/// Generic trap descriptor — either the toy `Aster.read(key, field)` API
/// the prototype shipped with, or a Convex async syscall (`Convex.asyncSyscall`)
/// matching the upstream backend's wire shape. The cell scheduler dispatches
/// on this enum and resolves the embedded `PromiseResolver` either way.
#[derive(Debug)]
enum PendingTrap {
    /// Legacy `Aster.read(key, field)` — point read for one document field.
    AsterRead {
        key: DocumentId,
        field: String,
        resolver: v8::Global<v8::PromiseResolver>,
    },
    /// `Convex.asyncSyscall(name, args_json_string)` — the Convex backend's
    /// real wire shape. v0.5 only handles `name == "1.0/get"`; everything
    /// else surfaces as a typed error (which becomes a JS exception via
    /// `resolver.reject`).
    ConvexSyscall {
        name: String,
        args_json: String,
        resolver: v8::Global<v8::PromiseResolver>,
    },
}

impl PendingTrap {
    fn resolver(&self) -> &v8::Global<v8::PromiseResolver> {
        match self {
            Self::AsterRead { resolver, .. } => resolver,
            Self::ConvexSyscall { resolver, .. } => resolver,
        }
    }
}

#[derive(Debug, Default)]
struct V8CellState {
    capsule: Option<SnapshotCapsule>,
    traps: VecDeque<PendingTrap>,
}

impl V8CellState {
    fn read_field(&self, key: &DocumentId, field: &str) -> Option<Value> {
        let capsule = self.capsule.as_ref()?;
        let versioned = capsule.get(key)?;
        let document = versioned.document.as_ref()?;
        document.get(field).cloned()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct V8ExecutionResult {
    pub output: Value,
    pub traps: usize,
    pub capsule_hash: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub enum V8CellError {
    WrongTenant,
    WrongDeployment,
    Compile(String),
    Run(String),
    NotAPromise,
    TooManyTraps { limit: usize },
    PendingWithoutTrap,
    Rejected(String),
    UnsupportedValue(String),
    Broker(String),
}

impl std::fmt::Display for V8CellError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongTenant => write!(f, "invocation tenant does not match V8 cell tenant"),
            Self::WrongDeployment => {
                write!(f, "invocation deployment does not match V8 cell deployment")
            }
            Self::Compile(error) => write!(f, "JavaScript compile error: {error}"),
            Self::Run(error) => write!(f, "JavaScript run error: {error}"),
            Self::NotAPromise => write!(f, "JavaScript entrypoint did not return a Promise"),
            Self::TooManyTraps { limit } => write!(f, "too many V8 read traps, limit {limit}"),
            Self::PendingWithoutTrap => {
                write!(f, "Promise is pending but no read trap was emitted")
            }
            Self::Rejected(error) => write!(f, "JavaScript promise rejected: {error}"),
            Self::UnsupportedValue(error) => write!(f, "unsupported JavaScript value: {error}"),
            Self::Broker(error) => write!(f, "broker error: {error}"),
        }
    }
}

impl std::error::Error for V8CellError {}

impl From<BrokerError> for V8CellError {
    fn from(value: BrokerError) -> Self {
        Self::Broker(value.to_string())
    }
}

/// A tenant/deployment pinned V8 cell.
///
/// The isolate is real and the broker may be in-process (unit tests) or a
/// UDS-backed process (`aster_v8cell`). The cell global object intentionally
/// exposes only the narrow read surfaces (`Aster.read` legacy plus
/// `Convex.asyncSyscall("1.0/get")`); no `fetch`, no timers, no filesystem,
/// and no database handle are installed.
pub struct V8SandboxCell {
    tenant: TenantId,
    deployment: DeploymentId,
    max_traps: usize,
}

/// What the namespace lookup yielded when the cell-side ESM loader
/// inspects the bundled module.
///
/// The Convex factory `query()`/`mutation()`/`action()` produces a
/// callable function with `isQuery|isMutation|isAction` flags hung off
/// it (see /tmp/aster-research-bundle-ground-truth.md §3.3). The cell
/// only runs queries in v0.5; the other shapes are rejected up front
/// rather than half-executed.
#[derive(Debug, Eq, PartialEq)]
enum ExportShape {
    Query,
    Mutation,
    Action,
    Other,
}

impl V8SandboxCell {
    pub fn new(tenant: TenantId, deployment: DeploymentId, max_traps: usize) -> Self {
        init_v8();
        Self {
            tenant,
            deployment,
            max_traps,
        }
    }

    /// Execute `source` as an async JS program named `main`.
    ///
    /// `main` must return a Promise. Missing reads suspend at `await
    /// Aster.read(key, field)`. This method drains all typed traps by reading
    /// from `store` at the original snapshot timestamp and resolving the exact
    /// V8 `PromiseResolver` that caused the trap.
    pub fn execute_async_main_with_broker(
        &self,
        broker: &impl CapsuleBrokerClient,
        cell_id: impl Into<String>,
        lease_epoch: u64,
        tenant: TenantId,
        deployment: DeploymentId,
        snapshot_ts: u64,
        prewarm: Vec<DocumentId>,
        source: &str,
    ) -> Result<V8ExecutionResult, V8CellError> {
        if tenant != self.tenant {
            return Err(V8CellError::WrongTenant);
        }
        if deployment != self.deployment {
            return Err(V8CellError::WrongDeployment);
        }

        let context = SealContext::new(cell_id, lease_epoch);
        let initial = broker.initial_capsule(&context, tenant, deployment, snapshot_ts, prewarm)?;
        let boxed_state = Box::new(Mutex::new(V8CellState {
            capsule: Some(initial.capsule().clone()),
            traps: VecDeque::new(),
        }));
        let state_ptr: *mut Mutex<V8CellState> = Box::into_raw(boxed_state);

        let result = unsafe {
            self.execute_with_broker_state_ptr(broker, &context, initial, state_ptr, source)
        };

        let state_box = unsafe { Box::from_raw(state_ptr) };
        match result {
            Ok(mut output) => {
                let state = state_box.lock().expect("v8 state mutex poisoned");
                let hash = state
                    .capsule
                    .as_ref()
                    .map(|capsule| capsule.root_hash)
                    .unwrap_or_default();
                output.capsule_hash = hash;
                Ok(output)
            }
            Err(error) => Err(error),
        }
    }

    pub fn execute_async_main(
        &self,
        store: &MvccStore,
        tenant: TenantId,
        deployment: DeploymentId,
        snapshot_ts: u64,
        prewarm: Vec<DocumentId>,
        source: &str,
    ) -> Result<V8ExecutionResult, V8CellError> {
        if tenant != self.tenant {
            return Err(V8CellError::WrongTenant);
        }
        if deployment != self.deployment {
            return Err(V8CellError::WrongDeployment);
        }

        let initial_capsule = store.build_capsule(tenant, deployment, snapshot_ts, prewarm);
        let boxed_state = Box::new(Mutex::new(V8CellState {
            capsule: Some(initial_capsule),
            traps: VecDeque::new(),
        }));
        let state_ptr: *mut Mutex<V8CellState> = Box::into_raw(boxed_state);

        // SAFETY: `state_ptr` is stored in one V8 External and all JS execution
        // is synchronous on this Rust thread. We reclaim the Box before return.
        let result = unsafe { self.execute_with_state_ptr(store, state_ptr, source) };

        // SAFETY: V8 is done with callbacks by this point; reclaim ownership.
        let state_box = unsafe { Box::from_raw(state_ptr) };
        match result {
            Ok(mut output) => {
                let state = state_box.lock().expect("v8 state mutex poisoned");
                let hash = state
                    .capsule
                    .as_ref()
                    .map(|capsule| capsule.root_hash)
                    .unwrap_or_default();
                output.capsule_hash = hash;
                Ok(output)
            }
            Err(error) => Err(error),
        }
    }

    /// Run a Convex-deploy bundled module's named query end to end.
    ///
    /// This is the entry point a v8cell binary uses when the broker hands it
    /// a real `npx convex deploy` bundle (one fully-flattened ESM JS string,
    /// no remaining `import`s — see
    /// `/tmp/aster-research-bundle-ground-truth.md` §4) plus a function name
    /// and a JSON-encoded args object. The cell:
    ///
    /// 1. Builds an isolate + context with the same global surface as
    ///    `execute_async_main_with_broker` (Aster.read, Convex.asyncSyscall),
    ///    plus a `Convex.syscall` stub that throws on call so the bundle's
    ///    own `performSyscall` guard at lines 906–913 of the bundle finds a
    ///    function (not `undefined`) when it lazy-checks the surface — see
    ///    `/tmp/aster-research-bundle-ground-truth.md` §3.1.
    /// 2. Compiles `module_source` as an ES module. Per
    ///    `/tmp/aster-research-bundle-ground-truth.md` §4 the bundle has zero
    ///    `import` statements, so the resolver callback is defensive only.
    /// 3. Instantiates and evaluates the module, pumping microtasks and
    ///    draining `Convex.asyncSyscall` traps via the existing trap dispatch
    ///    loop (the same path that backs `execute_async_main_with_broker`).
    /// 4. Reads `module.namespace[function_name]`. If absent, errors with the
    ///    typed `Run` variant listing the actually-available exports.
    /// 5. Validates the export's shape: must be a Convex `query()` (i.e.
    ///    `isQuery === true`). Mutations and actions are explicitly rejected
    ///    — Aster v0.5 cells run without DB write credentials. See
    ///    `/tmp/aster-research-convex-udf-runner.md` §8 D6.
    /// 6. Calls `<export>.invokeQuery(args_json_string)`, which returns a
    ///    `Promise<string>` in the bundle's wire shape (`registration_impl.ts:328-345`).
    /// 7. Drives that promise to fulfilment with the same trap loop, then
    ///    returns the resolved string verbatim as `Value::Text` — the JSON
    ///    on the wire is the JS-side `JSON.stringify(convexToJson(result))`,
    ///    matching what the upstream Convex backend would have sent.
    pub fn execute_module_query_with_broker(
        &self,
        broker: &impl CapsuleBrokerClient,
        cell_id: impl Into<String>,
        lease_epoch: u64,
        tenant: TenantId,
        deployment: DeploymentId,
        snapshot_ts: u64,
        prewarm: Vec<DocumentId>,
        module_source: &str,
        function_name: &str,
        args_json: &str,
    ) -> Result<V8ExecutionResult, V8CellError> {
        if tenant != self.tenant {
            return Err(V8CellError::WrongTenant);
        }
        if deployment != self.deployment {
            return Err(V8CellError::WrongDeployment);
        }

        let context = SealContext::new(cell_id, lease_epoch);
        let initial = broker.initial_capsule(&context, tenant, deployment, snapshot_ts, prewarm)?;
        let boxed_state = Box::new(Mutex::new(V8CellState {
            capsule: Some(initial.capsule().clone()),
            traps: VecDeque::new(),
        }));
        let state_ptr: *mut Mutex<V8CellState> = Box::into_raw(boxed_state);

        let result = unsafe {
            self.execute_module_query_with_broker_state_ptr(
                broker,
                &context,
                initial,
                state_ptr,
                module_source,
                function_name,
                args_json,
            )
        };

        let state_box = unsafe { Box::from_raw(state_ptr) };
        match result {
            Ok(mut output) => {
                let state = state_box.lock().expect("v8 state mutex poisoned");
                let hash = state
                    .capsule
                    .as_ref()
                    .map(|capsule| capsule.root_hash)
                    .unwrap_or_default();
                output.capsule_hash = hash;
                Ok(output)
            }
            Err(error) => Err(error),
        }
    }

    unsafe fn execute_module_query_with_broker_state_ptr(
        &self,
        broker: &impl CapsuleBrokerClient,
        context: &SealContext,
        mut sealed_capsule: aster_capsule::SealedCapsule,
        state_ptr: *mut Mutex<V8CellState>,
        module_source: &str,
        function_name: &str,
        args_json: &str,
    ) -> Result<V8ExecutionResult, V8CellError> {
        self.execute_module_core(state_ptr, module_source, function_name, args_json, |key| {
            sealed_capsule = broker.hydrate_point(context, sealed_capsule.clone(), key.clone())?;
            let capsule = sealed_capsule.capsule().clone();
            let state = &*state_ptr;
            state.lock().expect("v8 state mutex poisoned").capsule = Some(capsule);
            Ok(())
        })
    }

    unsafe fn execute_with_broker_state_ptr(
        &self,
        broker: &impl CapsuleBrokerClient,
        context: &SealContext,
        mut sealed_capsule: aster_capsule::SealedCapsule,
        state_ptr: *mut Mutex<V8CellState>,
        source: &str,
    ) -> Result<V8ExecutionResult, V8CellError> {
        self.execute_core(state_ptr, source, |key| {
            sealed_capsule = broker.hydrate_point(context, sealed_capsule.clone(), key.clone())?;
            let capsule = sealed_capsule.capsule().clone();
            let state = &*state_ptr;
            state.lock().expect("v8 state mutex poisoned").capsule = Some(capsule);
            Ok(())
        })
    }

    unsafe fn execute_with_state_ptr(
        &self,
        store: &MvccStore,
        state_ptr: *mut Mutex<V8CellState>,
        source: &str,
    ) -> Result<V8ExecutionResult, V8CellError> {
        self.execute_core(state_ptr, source, |key| {
            let ts = {
                let state = &*state_ptr;
                state
                    .lock()
                    .expect("v8 state mutex poisoned")
                    .capsule
                    .as_ref()
                    .expect("capsule present")
                    .ts
            };
            let value = store.read_at(key, ts);
            let state = &*state_ptr;
            let mut state = state.lock().expect("v8 state mutex poisoned");
            state
                .capsule
                .as_mut()
                .expect("capsule present")
                .hydrate_point(key.clone(), value);
            Ok(())
        })
    }

    unsafe fn execute_core(
        &self,
        state_ptr: *mut Mutex<V8CellState>,
        source: &str,
        mut hydrate: impl FnMut(&DocumentId) -> Result<(), V8CellError>,
    ) -> Result<V8ExecutionResult, V8CellError> {
        let create_params = v8::CreateParams::default();
        let mut isolate = v8::Isolate::new(create_params);
        // Host-controlled continuation: V8 should not decide when to drain
        // Promise jobs. The cell scheduler hydrates traps, resolves exactly one
        // host promise, then explicitly checkpoints microtasks.
        isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
        let mut hs = v8::HandleScope::new(&mut isolate);
        let scope = &mut hs;

        let global = v8::ObjectTemplate::new(scope);
        let external = v8::External::new(scope, state_ptr.cast::<c_void>());

        // `Aster.read(key, field)` — legacy toy API, kept for v0.3-era
        // tests and the `process_boundary` E2E. Not used by Convex apps.
        let read_template = v8::FunctionTemplate::builder(aster_read_callback)
            .data(external.into())
            .build(scope);
        let aster_template = v8::ObjectTemplate::new(scope);
        let read_name = v8::String::new(scope, "read").unwrap();
        aster_template.set(read_name.into(), read_template.into());
        let aster_name = v8::String::new(scope, "Aster").unwrap();
        global.set(aster_name.into(), aster_template.into());

        // `Convex.asyncSyscall(name, argsJson)` — the upstream Convex
        // backend's wire shape, matched verbatim so a Convex-compiled
        // function can `await ctx.db.get(id)` (which expands to
        // `performAsyncSyscall("1.0/get", {id})` in the user's bundle)
        // without modification. v0.5 only handles `"1.0/get"`.
        let convex_async_template = v8::FunctionTemplate::builder(convex_async_syscall_callback)
            .data(external.into())
            .build(scope);
        let convex_template = v8::ObjectTemplate::new(scope);
        let async_name = v8::String::new(scope, "asyncSyscall").unwrap();
        convex_template.set(async_name.into(), convex_async_template.into());
        let convex_name = v8::String::new(scope, "Convex").unwrap();
        global.set(convex_name.into(), convex_template.into());

        let context = v8::Context::new(
            scope,
            v8::ContextOptions {
                global_template: Some(global),
                ..Default::default()
            },
        );
        let scope = &mut v8::ContextScope::new(scope, context);

        let source = v8::String::new(scope, source).unwrap();
        let script = v8::Script::compile(scope, source, None)
            .ok_or_else(|| V8CellError::Compile("Script::compile returned None".to_string()))?;
        script
            .run(scope)
            .ok_or_else(|| V8CellError::Run("top-level script threw".to_string()))?;

        let main_name = v8::String::new(scope, "main").unwrap();
        let main_value = context
            .global(scope)
            .get(scope, main_name.into())
            .ok_or_else(|| V8CellError::Run("globalThis.main is missing".to_string()))?;
        let main_fn = v8::Local::<v8::Function>::try_from(main_value)
            .map_err(|_| V8CellError::Run("globalThis.main is not a function".to_string()))?;
        let undefined = v8::undefined(scope).into();
        let promise_value = main_fn.call(scope, undefined, &[]).ok_or_else(|| {
            V8CellError::Run("main() threw before returning a Promise".to_string())
        })?;
        let promise = v8::Local::<v8::Promise>::try_from(promise_value)
            .map_err(|_| V8CellError::NotAPromise)?;
        let promise_global = v8::Global::new(scope, promise);

        let mut traps = 0usize;
        loop {
            scope.perform_microtask_checkpoint();
            let promise = v8::Local::new(scope, &promise_global);
            match promise.state() {
                v8::PromiseState::Fulfilled => {
                    let value = promise.result(scope);
                    let output = v8_value_to_capsule_value(scope, value)?;
                    return Ok(V8ExecutionResult {
                        output,
                        traps,
                        capsule_hash: 0,
                    });
                }
                v8::PromiseState::Rejected => {
                    let value = promise.result(scope);
                    return Err(V8CellError::Rejected(value_to_string(scope, value)));
                }
                v8::PromiseState::Pending => {
                    let pending = {
                        let state = &*state_ptr;
                        state
                            .lock()
                            .expect("v8 state mutex poisoned")
                            .traps
                            .pop_front()
                    };
                    let Some(pending) = pending else {
                        return Err(V8CellError::PendingWithoutTrap);
                    };
                    if traps >= self.max_traps {
                        return Err(V8CellError::TooManyTraps {
                            limit: self.max_traps,
                        });
                    }
                    traps += 1;

                    // Dispatch the trap. AsterRead resolves with the Convex
                    // value at (key, field). Convex.asyncSyscall("1.0/get")
                    // resolves with a JSON string the JS side parses; v0.5
                    // returns the document's `_raw` field verbatim (the
                    // Postgres adapter put the upstream Convex JSON bytes
                    // there). Other syscall names reject with a typed
                    // V8CellError so the JS side sees a Promise rejection
                    // rather than a hung await.
                    let resolver = v8::Local::new(scope, pending.resolver());
                    match &pending {
                        PendingTrap::AsterRead { key, field, .. } => {
                            hydrate(key)?;
                            let value = {
                                let state = &*state_ptr;
                                let state = state.lock().expect("v8 state mutex poisoned");
                                state.read_field(key, field).unwrap_or(Value::Null)
                            };
                            let js_value = capsule_value_to_v8(scope, &value);
                            resolver.resolve(scope, js_value).ok_or_else(|| {
                                V8CellError::Run("PromiseResolver::resolve failed".to_string())
                            })?;
                        }
                        PendingTrap::ConvexSyscall {
                            name, args_json, ..
                        } if name == "1.0/get" => {
                            let id_str = parse_get_id(args_json)?;
                            let key = DocumentId::new(id_str);
                            hydrate(&key)?;
                            let json_str = doc_raw_as_json(state_ptr, &key);
                            let js_str = v8::String::new(scope, &json_str)
                                .unwrap_or_else(|| v8::String::empty(scope));
                            resolver.resolve(scope, js_str.into()).ok_or_else(|| {
                                V8CellError::Run("PromiseResolver::resolve failed".to_string())
                            })?;
                        }
                        PendingTrap::ConvexSyscall { name, .. } => {
                            // Unknown / unsupported syscall — surface as a
                            // JS exception so the user code can catch it.
                            let msg =
                                format!("aster-v8cell v0.5: unsupported convex syscall {name:?}");
                            let v8_msg = v8::String::new(scope, &msg).unwrap();
                            let err = v8::Exception::error(scope, v8_msg);
                            resolver.reject(scope, err).ok_or_else(|| {
                                V8CellError::Run("PromiseResolver::reject failed".to_string())
                            })?;
                        }
                    }
                }
            }
        }
    }

    /// ESM-flavoured sibling of `execute_core`. Compiles `module_source`
    /// as a V8 ES module, instantiates + evaluates it, then reads the
    /// requested export and dispatches `invokeQuery(args_json)` against it.
    /// Reuses the same trap-pump dispatch shape as `execute_core` so the
    /// existing `Convex.asyncSyscall("1.0/get", ...)` path keeps working
    /// inside an ESM body.
    ///
    /// The caller (`execute_module_query_with_broker_state_ptr`) supplies
    /// `hydrate` so the broker (real or in-process) can fulfil read traps
    /// without leaking the store handle into this method.
    unsafe fn execute_module_core(
        &self,
        state_ptr: *mut Mutex<V8CellState>,
        module_source: &str,
        function_name: &str,
        args_json: &str,
        mut hydrate: impl FnMut(&DocumentId) -> Result<(), V8CellError>,
    ) -> Result<V8ExecutionResult, V8CellError> {
        let create_params = v8::CreateParams::default();
        let mut isolate = v8::Isolate::new(create_params);
        // Same explicit microtask policy as the legacy path. ESM evaluation
        // queues a microtask for the module's body even when there's no
        // top-level `await`; we drain that with `perform_microtask_checkpoint`
        // inside the trap loop, identical to the legacy entry point.
        isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
        let mut hs = v8::HandleScope::new(&mut isolate);
        let scope = &mut hs;

        let global = v8::ObjectTemplate::new(scope);
        let external = v8::External::new(scope, state_ptr.cast::<c_void>());

        // Aster.read — kept on for parity with the legacy entry point so
        // the same isolate could run either shape. The Convex bundle
        // doesn't reference this global, so leaving it installed is free.
        let read_template = v8::FunctionTemplate::builder(aster_read_callback)
            .data(external.into())
            .build(scope);
        let aster_template = v8::ObjectTemplate::new(scope);
        let read_name = v8::String::new(scope, "read").unwrap();
        aster_template.set(read_name.into(), read_template.into());
        let aster_name = v8::String::new(scope, "Aster").unwrap();
        global.set(aster_name.into(), aster_template.into());

        // The Convex global. ESM bodies are strict-mode but globals still
        // resolve via the realm's global proxy — `Convex.asyncSyscall(...)`
        // inside the bundle hits this template, same as the legacy script
        // path. See /tmp/aster-research-rusty-v8-esm.md §7.
        let convex_template = v8::ObjectTemplate::new(scope);

        // `Convex.asyncSyscall(name, argsJson) -> Promise<string>`. The
        // bundle's `db.get(id)` flow lands here via `performAsyncSyscall`
        // (bundle lines 906–931, /tmp/aster-research-bundle-ground-truth.md §3.1).
        let convex_async_template = v8::FunctionTemplate::builder(convex_async_syscall_callback)
            .data(external.into())
            .build(scope);
        let async_name = v8::String::new(scope, "asyncSyscall").unwrap();
        convex_template.set(async_name.into(), convex_async_template.into());

        // `Convex.syscall(name, argsJson) -> string`. The bundle's
        // `performSyscall` (lines 905–913) lazy-checks
        // `Convex.syscall === void 0` and throws "outside of a Convex backend"
        // ONLY when called. The fixture's `db.get` path uses the async
        // surface only, so the stub is never invoked — it's installed so
        // the surface matches the bundle's expectations and so a future
        // user handler that reaches a sync syscall fails with a clear
        // typed JS error instead of the misleading "outside of a Convex
        // backend" string.
        let convex_sync_template = v8::FunctionTemplate::builder(convex_sync_syscall_stub_callback)
            .data(external.into())
            .build(scope);
        let sync_name = v8::String::new(scope, "syscall").unwrap();
        convex_template.set(sync_name.into(), convex_sync_template.into());

        let convex_name = v8::String::new(scope, "Convex").unwrap();
        global.set(convex_name.into(), convex_template.into());

        let context = v8::Context::new(
            scope,
            v8::ContextOptions {
                global_template: Some(global),
                ..Default::default()
            },
        );
        let scope = &mut v8::ContextScope::new(scope, context);

        // Compile the bundle as ES module. `is_module: true` is the 9th
        // boolean of `ScriptOrigin::new` — easy to miss; without it
        // `compile_module` returns None with a "Cannot use import statement
        // outside a module" error (memo /tmp/aster-research-rusty-v8-esm.md §9).
        let resource_name = v8::String::new(scope, "convex:/messages.js").unwrap();
        let origin = v8::ScriptOrigin::new(
            scope,
            resource_name.into(),
            /* line   */ 0,
            /* col    */ 0,
            /* shared */ false,
            /* script */ -1,
            /* sm url */ None,
            /* opaque */ false,
            /* wasm   */ false,
            /* module */ true,
            None,
        );
        let src_str = v8::String::new(scope, module_source)
            .ok_or_else(|| V8CellError::Compile("module source too large for V8".to_string()))?;
        let mut source = v8::script_compiler::Source::new(src_str, Some(&origin));
        let module = {
            let try_catch = &mut v8::TryCatch::new(scope);
            match v8::script_compiler::compile_module(try_catch, &mut source) {
                Some(m) => m,
                None => {
                    let err = try_catch
                        .exception()
                        .map(|v| value_to_string(try_catch, v))
                        .unwrap_or_else(|| "compile_module returned None".to_string());
                    return Err(V8CellError::Compile(err));
                }
            }
        };

        // Instantiate. The bundle is fully flattened by esbuild
        // (/tmp/aster-research-bundle-ground-truth.md §4 — `grep '^import '`
        // returns zero hits in the 2037-line bundle), so the resolver
        // should never fire. We pass a defensive stub that returns None
        // and surfaces a typed error if a future bundler emits an `import`.
        let ok = {
            let try_catch = &mut v8::TryCatch::new(scope);
            match module.instantiate_module(try_catch, resolve_unexpected_import_callback) {
                Some(b) => b,
                None => {
                    let err = try_catch
                        .exception()
                        .map(|v| value_to_string(try_catch, v))
                        .unwrap_or_else(|| {
                            "instantiate_module threw without exception".to_string()
                        });
                    return Err(V8CellError::Compile(format!(
                        "module instantiate failed: {err}"
                    )));
                }
            }
        };
        if !ok {
            return Err(V8CellError::Compile(
                "module instantiate_module returned false (resolver rejected an import — \
                 the bundle should be fully flattened by esbuild)"
                    .to_string(),
            ));
        }

        // Evaluate. Returns a Promise — even for modules without top-level
        // `await`, V8 wraps the resolution in a Promise for uniformity in
        // this binding. /tmp/aster-research-rusty-v8-esm.md §6.
        let eval_value = {
            let try_catch = &mut v8::TryCatch::new(scope);
            match module.evaluate(try_catch) {
                Some(v) => v,
                None => {
                    let err = try_catch
                        .exception()
                        .map(|v| value_to_string(try_catch, v))
                        .unwrap_or_else(|| "evaluate returned None".to_string());
                    return Err(V8CellError::Run(format!("module evaluate failed: {err}")));
                }
            }
        };
        let eval_promise = v8::Local::<v8::Promise>::try_from(eval_value).map_err(|_| {
            V8CellError::Run("module.evaluate did not return a Promise".to_string())
        })?;
        let eval_promise_global = v8::Global::new(scope, eval_promise);

        // Drive the evaluation promise to fulfilment via the standard
        // trap-pump loop. The bundle has no `await` on a host syscall at
        // top level (the fixture's only async work is inside `invokeQuery`),
        // so this loop typically completes in one microtask checkpoint
        // with zero traps consumed.
        let mut traps = 0usize;
        run_trap_pump_loop(
            scope,
            &eval_promise_global,
            state_ptr,
            self.max_traps,
            &mut traps,
            &mut hydrate,
        )?;
        // Defensive: a Module that hit Errored after Pending → Fulfilled
        // can't happen (V8 transitions Errored before resolving), but
        // assert anyway so we don't read a corrupt namespace.
        if module.get_status() != v8::ModuleStatus::Evaluated {
            return Err(V8CellError::Run(format!(
                "module status after evaluate is {:?}, expected Evaluated",
                module.get_status()
            )));
        }

        // Look up the requested export by name on the namespace object.
        // /tmp/aster-research-convex-udf-runner.md §3 — the runner reads
        // exactly `module.get_module_namespace().to_object(scope).get(name)`.
        let namespace = module
            .get_module_namespace()
            .to_object(scope)
            .ok_or_else(|| V8CellError::Run("module namespace was not an object".to_string()))?;
        let fn_key = v8::String::new(scope, function_name)
            .ok_or_else(|| V8CellError::Run("function name is not valid v8 string".to_string()))?;
        let has = namespace
            .has(scope, fn_key.into())
            .ok_or_else(|| V8CellError::Run("namespace.has threw".to_string()))?;
        if !has {
            // Enumerate available exports for the operator. Better debug
            // signal than "not found" — typo'd `getById` is the most
            // common failure mode here.
            let available = namespace
                .get_property_names(scope, v8::GetPropertyNamesArgsBuilder::default().build())
                .map(|arr| {
                    let mut out = Vec::with_capacity(arr.length() as usize);
                    for i in 0..arr.length() {
                        if let Some(v) = arr.get_index(scope, i) {
                            out.push(value_to_string(scope, v));
                        }
                    }
                    out
                })
                .unwrap_or_default();
            return Err(V8CellError::Run(format!(
                "module export {function_name:?} not found; available exports: {available:?}"
            )));
        }
        let export = namespace
            .get(scope, fn_key.into())
            .ok_or_else(|| V8CellError::Run("namespace.get returned None".to_string()))?;
        let export_obj = export.to_object(scope).ok_or_else(|| {
            V8CellError::Run(format!(
                "module export {function_name:?} is not an object \
                 (Convex query/mutation/action factories produce a callable object \
                 with isQuery/isMutation/isAction props — see registration_impl.ts:400-422)"
            ))
        })?;

        // Validate the export's shape. v0.5 cells only run queries.
        let shape = inspect_export_shape(scope, export_obj);
        match shape {
            ExportShape::Query => {}
            ExportShape::Mutation => {
                return Err(V8CellError::Run(format!(
                    "module export {function_name:?} is a mutation; \
                     Aster v0.5 cells only support query functions (no DB write capability)"
                )));
            }
            ExportShape::Action => {
                return Err(V8CellError::Run(format!(
                    "module export {function_name:?} is an action; \
                     Aster v0.5 cells only support query functions"
                )));
            }
            ExportShape::Other => {
                return Err(V8CellError::Run(format!(
                    "module export {function_name:?} is not a Convex query/mutation/action — \
                     missing isQuery/isMutation/isAction marker"
                )));
            }
        }

        // Read `<export>.invokeQuery` and call with the args JSON string.
        // /tmp/aster-research-bundle-ground-truth.md §3.3 + memo #3 §3.
        let invoke_key = v8::String::new(scope, "invokeQuery").unwrap();
        let invoke = export_obj
            .get(scope, invoke_key.into())
            .ok_or_else(|| V8CellError::Run("export has no invokeQuery property".to_string()))?;
        let invoke_fn = v8::Local::<v8::Function>::try_from(invoke).map_err(|_| {
            V8CellError::Run(format!(
                "export {function_name:?} has invokeQuery property but it is not a function"
            ))
        })?;
        let args_v8 = v8::String::new(scope, args_json)
            .ok_or_else(|| V8CellError::Run("args_json too large for v8 string".to_string()))?;

        let invoke_result = {
            let try_catch = &mut v8::TryCatch::new(scope);
            // Per /tmp/aster-research-convex-udf-runner.md §5, upstream
            // calls invoke with `global.into()` as `this`. Match that.
            let global_obj: v8::Local<v8::Value> = context.global(try_catch).into();
            match invoke_fn.call(try_catch, global_obj, &[args_v8.into()]) {
                Some(v) => v,
                None => {
                    let err = try_catch
                        .exception()
                        .map(|v| value_to_string(try_catch, v))
                        .unwrap_or_else(|| "invokeQuery threw without exception".to_string());
                    return Err(V8CellError::Run(format!(
                        "invokeQuery({function_name:?}) threw before returning a Promise: {err}"
                    )));
                }
            }
        };
        let invoke_promise = v8::Local::<v8::Promise>::try_from(invoke_result)
            .map_err(|_| V8CellError::NotAPromise)?;
        let invoke_promise_global = v8::Global::new(scope, invoke_promise);

        // Drive the invokeQuery promise — this is where the
        // `Convex.asyncSyscall("1.0/get", ...)` traps actually fire and
        // the broker hydrate path activates.
        run_trap_pump_loop(
            scope,
            &invoke_promise_global,
            state_ptr,
            self.max_traps,
            &mut traps,
            &mut hydrate,
        )?;

        // Pull the resolved string. invokeQuery returns
        // `JSON.stringify(convexToJson(result ?? null))` per
        // registration_impl.ts:344 — so the resolved value is always a
        // JSON-encoded ConvexValue string. We return it as Value::Text
        // unchanged; the broker / client decodes via aster-convex-codec.
        let promise = v8::Local::new(scope, &invoke_promise_global);
        let result_value = match promise.state() {
            v8::PromiseState::Fulfilled => promise.result(scope),
            v8::PromiseState::Rejected => {
                let rejected = promise.result(scope);
                return Err(V8CellError::Rejected(value_to_string(scope, rejected)));
            }
            v8::PromiseState::Pending => {
                return Err(V8CellError::PendingWithoutTrap);
            }
        };
        let result_str = if result_value.is_string() {
            value_to_string(scope, result_value)
        } else {
            // invokeQuery is contractually `Promise<string>` — anything
            // else is an upstream contract break.
            return Err(V8CellError::Run(format!(
                "invokeQuery resolved to non-string {:?}",
                value_to_string(scope, result_value)
            )));
        };

        Ok(V8ExecutionResult {
            output: Value::Text(result_str),
            traps,
            capsule_hash: 0,
        })
    }
}

/// Inspect the Convex query/mutation/action factory's marker properties.
/// /tmp/aster-research-bundle-ground-truth.md §3.3:
///
/// ```text
/// { isQuery|isMutation|isAction: true,
///   isPublic: true,
///   invokeQuery|invokeMutation|invokeAction: fn,
///   exportArgs, exportReturns, _handler }
/// ```
fn inspect_export_shape(scope: &mut v8::HandleScope, obj: v8::Local<v8::Object>) -> ExportShape {
    fn flag(scope: &mut v8::HandleScope, obj: v8::Local<v8::Object>, key: &str) -> bool {
        let Some(k) = v8::String::new(scope, key) else {
            return false;
        };
        let Some(v) = obj.get(scope, k.into()) else {
            return false;
        };
        v.is_true()
    }
    if flag(scope, obj, "isQuery") {
        ExportShape::Query
    } else if flag(scope, obj, "isMutation") {
        ExportShape::Mutation
    } else if flag(scope, obj, "isAction") {
        ExportShape::Action
    } else {
        ExportShape::Other
    }
}

/// Pump microtasks until `target` settles, draining traps along the way.
/// Shared between module evaluation and `invokeQuery` so a single promise
/// driver covers both legs of the bundle entry point.
unsafe fn run_trap_pump_loop(
    scope: &mut v8::HandleScope,
    target: &v8::Global<v8::Promise>,
    state_ptr: *mut Mutex<V8CellState>,
    max_traps: usize,
    traps: &mut usize,
    hydrate: &mut dyn FnMut(&DocumentId) -> Result<(), V8CellError>,
) -> Result<(), V8CellError> {
    loop {
        scope.perform_microtask_checkpoint();
        let promise = v8::Local::new(scope, target);
        match promise.state() {
            v8::PromiseState::Fulfilled => return Ok(()),
            v8::PromiseState::Rejected => {
                let rejected = promise.result(scope);
                return Err(V8CellError::Rejected(value_to_string(scope, rejected)));
            }
            v8::PromiseState::Pending => {
                let pending = {
                    let state = &*state_ptr;
                    state
                        .lock()
                        .expect("v8 state mutex poisoned")
                        .traps
                        .pop_front()
                };
                let Some(pending) = pending else {
                    return Err(V8CellError::PendingWithoutTrap);
                };
                if *traps >= max_traps {
                    return Err(V8CellError::TooManyTraps { limit: max_traps });
                }
                *traps += 1;

                let resolver = v8::Local::new(scope, pending.resolver());
                match &pending {
                    PendingTrap::AsterRead { key, field, .. } => {
                        hydrate(key)?;
                        let value = {
                            let state = &*state_ptr;
                            let state = state.lock().expect("v8 state mutex poisoned");
                            state.read_field(key, field).unwrap_or(Value::Null)
                        };
                        let js_value = capsule_value_to_v8(scope, &value);
                        resolver.resolve(scope, js_value).ok_or_else(|| {
                            V8CellError::Run("PromiseResolver::resolve failed".to_string())
                        })?;
                    }
                    PendingTrap::ConvexSyscall {
                        name, args_json, ..
                    } if name == "1.0/get" => {
                        let id_str = parse_get_id(args_json)?;
                        let key = DocumentId::new(id_str);
                        hydrate(&key)?;
                        let json_str = doc_raw_as_json(state_ptr, &key);
                        let js_str = v8::String::new(scope, &json_str)
                            .unwrap_or_else(|| v8::String::empty(scope));
                        resolver.resolve(scope, js_str.into()).ok_or_else(|| {
                            V8CellError::Run("PromiseResolver::resolve failed".to_string())
                        })?;
                    }
                    PendingTrap::ConvexSyscall { name, .. } => {
                        let msg = format!("aster-v8cell v0.5: unsupported convex syscall {name:?}");
                        let v8_msg = v8::String::new(scope, &msg).unwrap();
                        let err = v8::Exception::error(scope, v8_msg);
                        resolver.reject(scope, err).ok_or_else(|| {
                            V8CellError::Run("PromiseResolver::reject failed".to_string())
                        })?;
                    }
                }
            }
        }
    }
}

/// Defensive resolver used when the bundle ought to be self-contained.
/// `extern "C" fn` cannot capture (memo /tmp/aster-research-rusty-v8-esm.md §9),
/// so we can't smuggle host state in here; instead we throw a JS error
/// describing what happened so the operator sees a clear failure mode.
/// Returns `None` to signal resolution failure to V8.
fn resolve_unexpected_import_callback<'s>(
    context: v8::Local<'s, v8::Context>,
    specifier: v8::Local<'s, v8::String>,
    _import_attrs: v8::Local<'s, v8::FixedArray>,
    _referrer: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Module>> {
    // SAFETY: V8 invoked us inside `instantiate_module` on this isolate.
    let scope = &mut unsafe { v8::CallbackScope::new(context) };
    let spec = specifier.to_rust_string_lossy(scope);
    let msg = format!(
        "aster-v8cell v0.5: bundle has unexpected `import {spec:?}` — \
         the cell expects a fully flattened esbuild output \
         (see /tmp/aster-research-bundle-ground-truth.md §4)"
    );
    let v8_msg = v8::String::new(scope, &msg).unwrap();
    let err = v8::Exception::error(scope, v8_msg);
    scope.throw_exception(err);
    None
}

fn aster_read_callback(
    scope: &mut v8::HandleScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let Some(external) = v8::Local::<v8::External>::try_from(args.data()).ok() else {
        throw(scope, "Aster.read missing host state");
        return;
    };
    let state_ptr = external.value() as *mut Mutex<V8CellState>;
    if state_ptr.is_null() {
        throw(scope, "Aster.read null host state");
        return;
    }

    let key = match args.get(0).to_string(scope) {
        Some(value) => value.to_rust_string_lossy(scope),
        None => {
            throw(scope, "Aster.read key must be string-like");
            return;
        }
    };
    let field = match args.get(1).to_string(scope) {
        Some(value) => value.to_rust_string_lossy(scope),
        None => {
            throw(scope, "Aster.read field must be string-like");
            return;
        }
    };
    let key = DocumentId::new(key);

    let state = unsafe { &*state_ptr };
    if let Some(value) = state
        .lock()
        .expect("v8 state mutex poisoned")
        .read_field(&key, &field)
    {
        let value = capsule_value_to_v8(scope, &value);
        rv.set(value);
        return;
    }

    let Some(resolver) = v8::PromiseResolver::new(scope) else {
        throw(scope, "failed to allocate V8 PromiseResolver");
        return;
    };
    let promise = resolver.get_promise(scope);
    let resolver_global = v8::Global::new(scope, resolver);
    state
        .lock()
        .expect("v8 state mutex poisoned")
        .traps
        .push_back(PendingTrap::AsterRead {
            key,
            field,
            resolver: resolver_global,
        });
    rv.set(promise.into());
}

/// JS callback for `Convex.asyncSyscall(name, args_json_string)`. Mirrors
/// the upstream backend's wire shape so a Convex-compiled module can call
/// `db.get(id)` (which expands to `performAsyncSyscall("1.0/get", ...)`)
/// against an Aster cell without modification.
///
/// The cell scheduler dispatches on `name`: only `"1.0/get"` is wired
/// today; anything else surfaces as a typed JS rejection. The JS side
/// receives the resolved JSON string via `await`, then parses it
/// (Convex's `jsonToConvex` runs JS-side; Aster's host doesn't need to).
fn convex_async_syscall_callback(
    scope: &mut v8::HandleScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let Some(external) = v8::Local::<v8::External>::try_from(args.data()).ok() else {
        throw(scope, "Convex.asyncSyscall missing host state");
        return;
    };
    let state_ptr = external.value() as *mut Mutex<V8CellState>;
    if state_ptr.is_null() {
        throw(scope, "Convex.asyncSyscall null host state");
        return;
    }

    let name = match args.get(0).to_string(scope) {
        Some(value) => value.to_rust_string_lossy(scope),
        None => {
            throw(scope, "Convex.asyncSyscall name must be string-like");
            return;
        }
    };
    // Convex's JS shim sends the args object as a stringified JSON. For
    // the v0.5 Aster path we also accept a raw JS object — convert with
    // JSON.stringify on the host side via v8's `to_string`.
    let args_json = match args.get(1).to_string(scope) {
        Some(value) => value.to_rust_string_lossy(scope),
        None => {
            throw(
                scope,
                "Convex.asyncSyscall args must be string-like or stringifiable",
            );
            return;
        }
    };

    let Some(resolver) = v8::PromiseResolver::new(scope) else {
        throw(scope, "failed to allocate V8 PromiseResolver");
        return;
    };
    let promise = resolver.get_promise(scope);
    let resolver_global = v8::Global::new(scope, resolver);
    let state = unsafe { &*state_ptr };
    state
        .lock()
        .expect("v8 state mutex poisoned")
        .traps
        .push_back(PendingTrap::ConvexSyscall {
            name,
            args_json,
            resolver: resolver_global,
        });
    rv.set(promise.into());
}

/// JS callback for `Convex.syscall(name, argsJson)`. The bundle's
/// `performSyscall` (lines 905–913 of /tmp/aster-research/bundle-out/isolate/messages.js)
/// lazy-checks `Convex.syscall === void 0` and only then throws — so an
/// installed-but-throwing function is observably the same as "feature
/// unsupported, here's a clear error" from the user-handler's POV. The
/// fixture's `db.get` path doesn't reach this; it's installed for
/// surface-completeness and so a future user handler that does call
/// `performSyscall` gets a typed JS exception rather than the bundle's
/// misleading default message about "outside of a Convex backend".
fn convex_sync_syscall_stub_callback(
    scope: &mut v8::HandleScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let name = match args.get(0).to_string(scope) {
        Some(value) => value.to_rust_string_lossy(scope),
        None => "<unstringifiable>".to_string(),
    };
    let msg = format!(
        "aster-v8cell v0.5: synchronous Convex.syscall({name:?}) is not implemented — \
         only Convex.asyncSyscall(\"1.0/get\", ...) is wired in this cell"
    );
    throw(scope, &msg);
}

/// Extract `id` from a `Convex.asyncSyscall("1.0/get", argsJson)` payload.
/// Convex's JS shim sends this as `JSON.stringify({ id, isSystem, ...})`;
/// we only care about `id`. The broker accepts either Aster's
/// `<table_hex>/<id_hex>` wire form or a Convex IDv6 string.
fn parse_get_id(args_json: &str) -> Result<String, V8CellError> {
    let v: serde_json::Value = serde_json::from_str(args_json)
        .map_err(|err| V8CellError::Run(format!("convex 1.0/get bad JSON args: {err}")))?;
    let id = v
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            V8CellError::Run("convex 1.0/get: missing string field `id` in args".to_string())
        })?
        .to_string();
    Ok(id)
}

/// Pull the document out of the cell's hydrated capsule and return it as
/// a JSON string the JS side can `JSON.parse`. v0.5 keeps the bytes as
/// the Postgres adapter wrote them — `_raw` carries the upstream Convex
/// `json_value` blob untouched. Missing or tombstoned docs become
/// `"null"` so JS sees `await Convex.asyncSyscall("1.0/get", ...) === null`.
unsafe fn doc_raw_as_json(state_ptr: *mut Mutex<V8CellState>, key: &DocumentId) -> String {
    let state = &*state_ptr;
    let state = state.lock().expect("v8 state mutex poisoned");
    let raw = state
        .capsule
        .as_ref()
        .and_then(|capsule| capsule.get(key))
        .and_then(|versioned| versioned.document.as_ref())
        .and_then(|doc| doc.get("_raw"))
        .cloned();
    match raw {
        Some(Value::Text(s)) => s,
        _ => "null".to_string(),
    }
}

fn throw(scope: &mut v8::HandleScope, message: &str) {
    let message = v8::String::new(scope, message).unwrap();
    let error = v8::Exception::error(scope, message);
    scope.throw_exception(error);
}

fn capsule_value_to_v8<'s>(
    scope: &mut v8::HandleScope<'s>,
    value: &Value,
) -> v8::Local<'s, v8::Value> {
    match value {
        Value::Int(value) => v8::Number::new(scope, *value as f64).into(),
        Value::Text(value) => v8::String::new(scope, value).unwrap().into(),
        Value::Bool(value) => v8::Boolean::new(scope, *value).into(),
        Value::Null => v8::null(scope).into(),
    }
}

fn v8_value_to_capsule_value(
    scope: &mut v8::HandleScope,
    value: v8::Local<v8::Value>,
) -> Result<Value, V8CellError> {
    if value.is_int32() {
        Ok(Value::Int(
            value.int32_value(scope).unwrap_or_default() as i64
        ))
    } else if value.is_number() {
        Ok(Value::Int(
            value.number_value(scope).unwrap_or_default() as i64
        ))
    } else if value.is_boolean() {
        Ok(Value::Bool(value.boolean_value(scope)))
    } else if value.is_string() {
        Ok(Value::Text(value_to_string(scope, value)))
    } else if value.is_null_or_undefined() {
        Ok(Value::Null)
    } else {
        Err(V8CellError::UnsupportedValue(value_to_string(scope, value)))
    }
}

fn value_to_string(scope: &mut v8::HandleScope, value: v8::Local<v8::Value>) -> String {
    value
        .to_string(scope)
        .map(|value| value.to_rust_string_lossy(scope))
        .unwrap_or_else(|| "<unprintable>".to_string())
}

/// Helper used by tests and future host integration. It keeps this crate's
/// public surface free of V8 types.
pub fn document_from_i64(field: &str, value: i64) -> Document {
    let mut document = BTreeMap::new();
    document.insert(field.to_string(), Value::Int(value));
    document
}

#[cfg(test)]
mod tests {
    use super::*;
    use aster_capsule::doc_with_i64;

    #[test]
    fn v8_async_function_resumes_through_broker_without_store_handle() {
        let tenant = TenantId::new("tenant-v8-broker");
        let deployment = DeploymentId::new("dep-v8-broker");
        let store = MvccStore::new();
        store.seed(DocumentId::new("counters/a"), doc_with_i64("value", 20));
        store.seed(DocumentId::new("counters/b"), doc_with_i64("value", 22));
        let ts = store.snapshot_ts();
        let broker = aster_broker::LocalCapsuleBroker::new(
            &store,
            aster_capsule::CapsuleSealKey::derive_for_tests(b"v8-broker-test"),
        );

        let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);
        let source = r#"
            async function main() {
              const a = await Aster.read("counters/a", "value");
              const b = await Aster.read("counters/b", "value");
              return a + b;
            }
        "#;
        let result = cell
            .execute_async_main_with_broker(
                &broker,
                "cell-v8-1",
                3,
                tenant,
                deployment,
                ts,
                vec![DocumentId::new("counters/a")],
                source,
            )
            .expect("V8 async function should complete through broker");
        assert_eq!(result.output, Value::Int(42));
        assert_eq!(result.traps, 1);
        assert_ne!(result.capsule_hash, 0);
    }

    #[test]
    fn v8_async_function_resumes_after_read_trap() {
        let tenant = TenantId::new("tenant-v8");
        let deployment = DeploymentId::new("dep-v8");
        let store = MvccStore::new();
        store.seed(DocumentId::new("counters/a"), doc_with_i64("value", 40));
        store.seed(DocumentId::new("counters/b"), doc_with_i64("value", 2));
        let ts = store.snapshot_ts();

        let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);
        let source = r#"
            async function main() {
              const a = await Aster.read("counters/a", "value");
              const b = await Aster.read("counters/b", "value");
              return a + b;
            }
        "#;
        let result = cell
            .execute_async_main(
                &store,
                tenant,
                deployment,
                ts,
                vec![DocumentId::new("counters/a")],
                source,
            )
            .expect("V8 async function should complete");
        assert_eq!(result.output, Value::Int(42));
        assert_eq!(result.traps, 1);
        assert_ne!(result.capsule_hash, 0);
    }

    #[test]
    fn v8_convex_async_syscall_get_returns_doc_raw_as_json() {
        // Build a doc whose `_raw` field carries a JSON blob the way
        // PostgresCapsuleStore would after reading from convex.documents.
        // The JS function fires `Convex.asyncSyscall("1.0/get", ...)`,
        // gets the raw JSON string back, parses it, and returns one
        // field — proving the wire shape matches what a Convex-compiled
        // module would do via `await ctx.db.get(id)`.
        let tenant = TenantId::new("tenant-convex");
        let deployment = DeploymentId::new("dep-convex");
        let store = MvccStore::new();
        let doc_id = DocumentId::new("aabb/ccdd");
        let mut doc = aster_capsule::Document::new();
        doc.insert(
            "_raw".to_string(),
            Value::Text(r#"{"name":"ian","_id":"aabb/ccdd"}"#.to_string()),
        );
        store.seed(doc_id.clone(), doc);
        let ts = store.snapshot_ts();

        let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);
        let source = r#"
            async function main() {
              const json = await Convex.asyncSyscall(
                "1.0/get",
                JSON.stringify({ id: "aabb/ccdd", isSystem: false })
              );
              const doc = JSON.parse(json);
              return doc.name;
            }
        "#;
        let result = cell
            .execute_async_main(&store, tenant, deployment, ts, vec![], source)
            .expect("Convex.asyncSyscall(1.0/get) should complete");
        assert_eq!(result.output, Value::Text("ian".to_string()));
        assert_eq!(result.traps, 1, "exactly one async syscall trap");
    }

    #[test]
    fn v8_convex_async_syscall_unsupported_name_rejects_promise() {
        // Anything other than the v0.5-supported syscall names becomes a
        // typed JS rejection. The cell scheduler still completes
        // (V8 propagates the rejection to top-level main) — we observe
        // the rejection as a Run/Rejected V8CellError on the host side.
        let tenant = TenantId::new("tenant-rej");
        let deployment = DeploymentId::new("dep-rej");
        let store = MvccStore::new();
        let ts = store.snapshot_ts();
        let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);
        let source = r#"
            async function main() {
              return await Convex.asyncSyscall("1.0/totally-fake", "{}");
            }
        "#;
        let err = cell
            .execute_async_main(&store, tenant, deployment, ts, vec![], source)
            .expect_err("unsupported syscall must reject");
        match err {
            V8CellError::Rejected(msg) => {
                assert!(
                    msg.contains("unsupported convex syscall"),
                    "rejection should name the syscall, got {msg:?}"
                );
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }
}
