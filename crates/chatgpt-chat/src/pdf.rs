//! Resource-bounded structural PDF validation. Only our fixed, import-free Rust
//! validator is executed; PDF contents are data in its isolated linear memory.

use crate::attachment::AttachmentError;
use crate::attachment::MAX_SUBMISSION_BYTES;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use wasmi::CompilationMode;
use wasmi::Config;
use wasmi::Engine;
use wasmi::Instance;
use wasmi::Linker;
use wasmi::Module;
use wasmi::ResourceLimiter;
use wasmi::Store;
use wasmi::StoreLimits;
use wasmi::StoreLimitsBuilder;
use wasmi::TrapCode;
use wasmi::TypedResumableCall;
use wasmi_core::LimiterError;
use wasmi_core::MemoryError;
use wasmi_core::TableError;

const VALIDATOR: &[u8] = include_bytes!("../pdf-validator/validator.wasm");
const MAX_MEMORY_BYTES: usize = 128 * 1024 * 1024;
const FUEL_QUANTUM: u64 = 100_000;
const MAX_FUEL: u64 = 200_000_000;
const MAX_DURATION: Duration = Duration::from_secs(5);
static MODULE: OnceLock<Result<Module, AttachmentError>> = OnceLock::new();

pub(crate) fn validate(bytes: &[u8], cancel: &CancellationToken) -> Result<(), AttachmentError> {
    if cancel.is_cancelled() {
        return Err(AttachmentError::Cancelled);
    }
    if bytes.len() > MAX_SUBMISSION_BYTES {
        return Err(AttachmentError::Limit);
    }
    if !bytes.starts_with(b"%PDF-") {
        return Err(AttachmentError::InvalidFormat);
    }
    // Compile trusted code once on the decoder worker. Compilation does not
    // inspect input and uses eager translation so fuel yields are resumable.
    let module = MODULE
        .get_or_init(|| compile(VALIDATOR))
        .as_ref()
        .map_err(|error| *error)?;
    let mut budget = Budget::new(cancel);
    let (mut store, instance) = instantiate(module)?;
    budget.refill(&mut store, FUEL_QUANTUM)?;
    let allocate = instance
        .get_typed_func::<u32, u32>(&store, "allocate")
        .map_err(trap)?;
    let call = allocate
        .call_resumable(&mut store, bytes.len() as u32)
        .map_err(trap)?;
    let pointer = budget.finish(call, &mut store)?;
    if pointer == 0 {
        return Err(AttachmentError::Limit);
    }
    instance
        .get_memory(&store, "memory")
        .ok_or(AttachmentError::InvalidFormat)?
        .write(&mut store, pointer as usize, bytes)
        .map_err(|_| AttachmentError::InvalidFormat)?;
    budget.check()?;
    let validate = instance
        .get_typed_func::<(u32, u32), u32>(&store, "validate")
        .map_err(trap)?;
    let call = validate
        .call_resumable(&mut store, (pointer, bytes.len() as u32))
        .map_err(trap)?;
    match budget.finish(call, &mut store)? {
        0 => Ok(()),
        2 => Err(AttachmentError::Limit),
        _ => Err(AttachmentError::InvalidFormat),
    }
}

fn compile(bytes: &[u8]) -> Result<Module, AttachmentError> {
    let mut config = Config::default();
    config
        .compilation_mode(CompilationMode::Eager)
        .allow_start_fn(false)
        .consume_fuel(true)
        .set_max_recursion_depth(256)
        .set_max_stack_height(128 * 1024)
        .set_max_cached_stacks(1);
    let engine = Engine::new(&config);
    let module = Module::new(&engine, bytes).map_err(trap)?;
    // No WASI, JavaScript, clocks, logging, randomness, or host callbacks.
    if module.imports().count() != 0 {
        return Err(AttachmentError::InvalidFormat);
    }
    Ok(module)
}

fn instantiate(module: &Module) -> Result<(Store<Limits>, Instance), AttachmentError> {
    let limits = StoreLimitsBuilder::new()
        .memory_size(MAX_MEMORY_BYTES)
        .memories(1)
        .instances(1)
        .tables(1)
        .table_elements(2048)
        .trap_on_grow_failure(true)
        .build();
    let mut store = Store::new(module.engine(), Limits(limits));
    store.limiter(|limits| limits);
    let instance = Linker::new(module.engine())
        .instantiate_and_start(&mut store, module)
        .map_err(trap)?;
    Ok((store, instance))
}

struct Limits(StoreLimits);

impl ResourceLimiter for Limits {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> Result<bool, LimiterError> {
        self.0.memory_growing(current, desired, maximum)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> Result<bool, LimiterError> {
        self.0.table_growing(current, desired, maximum)
    }

    fn memory_grow_failed(&mut self, error: &MemoryError) -> Result<(), LimiterError> {
        // Wasmi reports a fuel yield through this hook as well as allocation
        // failures. StoreLimits' trap-on-failure would turn that resumable yield
        // into a terminal trap, rejecting valid large input allocations.
        if matches!(error, MemoryError::OutOfFuel { .. }) {
            return Ok(());
        }
        self.0.memory_grow_failed(error)
    }

    fn table_grow_failed(&mut self, error: &TableError) -> Result<(), LimiterError> {
        if matches!(error, TableError::OutOfFuel { .. }) {
            return Ok(());
        }
        self.0.table_grow_failed(error)
    }

    fn instances(&self) -> usize {
        self.0.instances()
    }

    fn tables(&self) -> usize {
        self.0.tables()
    }

    fn memories(&self) -> usize {
        self.0.memories()
    }
}

struct Budget<'a> {
    cancel: &'a CancellationToken,
    deadline: Instant,
    remaining: u64,
}

impl<'a> Budget<'a> {
    fn new(cancel: &'a CancellationToken) -> Self {
        Self {
            cancel,
            deadline: Instant::now() + MAX_DURATION,
            remaining: MAX_FUEL,
        }
    }

    fn check(&self) -> Result<(), AttachmentError> {
        if self.cancel.is_cancelled() {
            return Err(AttachmentError::Cancelled);
        }
        if Instant::now() >= self.deadline {
            return Err(AttachmentError::Limit);
        }
        Ok(())
    }

    fn refill(&mut self, store: &mut Store<Limits>, required: u64) -> Result<(), AttachmentError> {
        self.check()?;
        // Bulk memory operations cost more than one quantum. They remain
        // bounded by linear-memory limits and the total fuel allowance.
        let granted = required.max(FUEL_QUANTUM);
        self.remaining = self
            .remaining
            .checked_sub(granted)
            .ok_or(AttachmentError::Limit)?;
        store.set_fuel(granted).map_err(trap)
    }

    fn finish(
        &mut self,
        mut call: TypedResumableCall<u32>,
        store: &mut Store<Limits>,
    ) -> Result<u32, AttachmentError> {
        loop {
            self.check()?;
            match call {
                TypedResumableCall::Finished(value) => return Ok(value),
                TypedResumableCall::HostTrap(_) => return Err(AttachmentError::InvalidFormat),
                TypedResumableCall::OutOfFuel(yielded) => {
                    self.refill(store, yielded.required_fuel())?;
                    call = yielded.resume(&mut *store).map_err(trap)?;
                }
            }
        }
    }
}

fn trap(error: wasmi::Error) -> AttachmentError {
    match error.as_trap_code() {
        Some(
            TrapCode::GrowthOperationLimited
            | TrapCode::StackOverflow
            | TrapCode::OutOfFuel
            | TrapCode::OutOfSystemMemory,
        ) => AttachmentError::Limit,
        _ => AttachmentError::InvalidFormat,
    }
}

#[cfg(test)]
mod tests;
