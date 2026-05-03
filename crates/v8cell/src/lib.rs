//! V8-backed read-trap continuation proof of concept for Aster v0.2.
//!
//! This crate answers the load-bearing v0.1 question: can a tenant JavaScript
//! function suspend on a missing capsule read, let the host hydrate data, and
//! resume inside a real V8 isolate without rebuilding V8's scheduler?
//!
//! The answer demonstrated here is "yes, if the continuation boundary is an
//! `await` over a host-created Promise". We do not attempt to capture arbitrary
//! synchronous JS stacks. The host API `Aster.read(key, field)` returns a value
//! immediately for warm capsule entries and returns a pending Promise for a
//! missing read. V8 preserves the async continuation. The Rust host receives a
//! typed [`V8ReadTrap`], hydrates the capsule through its broker, resolves the
//! promise, runs a microtask checkpoint, and the same JS async function returns
//! the final value.

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

#[derive(Debug)]
struct PendingRead {
    key: DocumentId,
    field: String,
    resolver: v8::Global<v8::PromiseResolver>,
}

#[derive(Debug, Default)]
struct V8CellState {
    capsule: Option<SnapshotCapsule>,
    traps: VecDeque<PendingRead>,
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
/// The prototype keeps the broker in-process, but the isolate itself is real.
/// The cell global object intentionally exposes only `Aster.read`; no `fetch`,
/// no timers, no filesystem, and no database handle are installed.
pub struct V8SandboxCell {
    tenant: TenantId,
    deployment: DeploymentId,
    max_traps: usize,
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

    unsafe fn execute_with_broker_state_ptr(
        &self,
        broker: &impl CapsuleBrokerClient,
        context: &SealContext,
        mut sealed_capsule: aster_capsule::SealedCapsule,
        state_ptr: *mut Mutex<V8CellState>,
        source: &str,
    ) -> Result<V8ExecutionResult, V8CellError> {
        self.execute_core(state_ptr, source, |pending| {
            sealed_capsule =
                broker.hydrate_point(context, sealed_capsule.clone(), pending.key.clone())?;
            let capsule = sealed_capsule.capsule().clone();
            {
                let state = &*state_ptr;
                state.lock().expect("v8 state mutex poisoned").capsule = Some(capsule);
            }
            let resolved = {
                let state = &*state_ptr;
                let state = state.lock().expect("v8 state mutex poisoned");
                state
                    .read_field(&pending.key, &pending.field)
                    .unwrap_or(Value::Null)
            };
            Ok(resolved)
        })
    }

    unsafe fn execute_with_state_ptr(
        &self,
        store: &MvccStore,
        state_ptr: *mut Mutex<V8CellState>,
        source: &str,
    ) -> Result<V8ExecutionResult, V8CellError> {
        self.execute_core(state_ptr, source, |pending| {
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
            let value = store.read_at(&pending.key, ts);
            {
                let state = &*state_ptr;
                let mut state = state.lock().expect("v8 state mutex poisoned");
                state
                    .capsule
                    .as_mut()
                    .expect("capsule present")
                    .hydrate_point(pending.key.clone(), value);
            }
            let resolved = {
                let state = &*state_ptr;
                let state = state.lock().expect("v8 state mutex poisoned");
                state
                    .read_field(&pending.key, &pending.field)
                    .unwrap_or(Value::Null)
            };
            Ok(resolved)
        })
    }

    unsafe fn execute_core(
        &self,
        state_ptr: *mut Mutex<V8CellState>,
        source: &str,
        mut hydrate: impl FnMut(&PendingRead) -> Result<Value, V8CellError>,
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
        let read_template = v8::FunctionTemplate::builder(aster_read_callback)
            .data(external.into())
            .build(scope);
        let aster_template = v8::ObjectTemplate::new(scope);
        let read_name = v8::String::new(scope, "read").unwrap();
        aster_template.set(read_name.into(), read_template.into());
        let aster_name = v8::String::new(scope, "Aster").unwrap();
        global.set(aster_name.into(), aster_template.into());

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

                    let resolved = hydrate(&pending)?;
                    let resolver = v8::Local::new(scope, &pending.resolver);
                    let js_value = capsule_value_to_v8(scope, &resolved);
                    resolver.resolve(scope, js_value).ok_or_else(|| {
                        V8CellError::Run("PromiseResolver::resolve failed".to_string())
                    })?;
                }
            }
        }
    }
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
        .push_back(PendingRead {
            key,
            field,
            resolver: resolver_global,
        });
    rv.set(promise.into());
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
}
