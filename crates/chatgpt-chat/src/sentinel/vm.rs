//! Register machine with an arena, so cyclic guest objects cannot leak memory.
//! Only the explicitly implemented host values below are visible to programs.

use super::Budget;
use super::Limits;
use super::Profile;
use super::Result;
use super::SentinelFailure as Error;
use super::layout::NativeLayout;
use super::profile::DEVICE_MEMORY_GIB;
use super::profile::HARDWARE_CONCURRENCY;
use super::profile::JS_HEAP_LIMIT;
use super::profile::SCREEN_COLOR_DEPTH;
use super::profile::SCREEN_HEIGHT;
use super::profile::SCREEN_WIDTH;
use super::profile::USER_AGENT;
use super::storage::StorageKind;
use super::style::InlineStyle;
use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use serde_json::Value as Json;
use std::collections::BTreeMap;
use std::ops::Deref;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
enum Value {
    Undefined,
    // A declared, absent application global can be tested for presence. Its
    // payload remains unavailable: use must fail, never synthesize a value or
    // turn the error into a successful challenge result.
    AbsentRouterContext,
    Null,
    Bool(bool),
    Number(f64),
    Heap(usize),
    Opcode(u8),
    Host(Host),
    Builtin(Builtin),
    Registers,
}

impl Value {
    fn required(self) -> Result<Self> {
        match self {
            Self::AbsentRouterContext => Err(Error::RequiredRouterContextUnavailable),
            value => Ok(value),
        }
    }
}

enum Heap {
    Text(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
    NullObject(Vec<(String, Value)>),
    // Arena objects already live until this VM is dropped. Membership retains
    // identity without granting access to a host collector or global object.
    WeakSet(Vec<Value>),
    Bound {
        function: Value,
        receiver: Value,
        arguments: Vec<Value>,
    },
    Canvas {
        width: u32,
        height: u32,
    },
    Rect([f64; 4]),
    Element {
        kind: Element,
        style: Value,
        id: String,
        class_name: String,
        text_content: String,
        aria_hidden: Option<String>,
        connected: bool,
        has_line_breaks: bool,
    },
    Style(InlineStyle),
    Callback {
        result: Value,
        args: Vec<Value>,
        program: Value,
    },
}

#[derive(Clone, Copy)]
enum Element {
    Div,
    Span,
    Body,
    Html,
    Anchor,
    Input,
}

impl Element {
    fn tag(self) -> &'static str {
        match self {
            Self::Div => "DIV",
            Self::Span => "SPAN",
            Self::Body => "BODY",
            Self::Html => "HTML",
            Self::Anchor => "A",
            Self::Input => "INPUT",
        }
    }

    fn interface(self) -> &'static str {
        match self {
            Self::Div => "HTMLDivElement",
            Self::Span => "HTMLSpanElement",
            Self::Body => "HTMLBodyElement",
            Self::Html => "HTMLHtmlElement",
            Self::Anchor => "HTMLAnchorElement",
            Self::Input => "HTMLInputElement",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Host {
    Window,
    Navigator,
    Document,
    RouterContext,
    DocumentElement,
    Body,
    Dataset,
    Location,
    Performance,
    Date,
    Memory,
    Screen,
    ScreenOrientation,
    History,
    Math,
    Object,
    Reflect,
    Json,
    Storage(StorageKind),
    StorageManager,
    WeakSetConstructor,
    WeakSetPrototype,
    Number,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Builtin {
    Random,
    Now,
    EpochNow,
    Hypot,
    Sqrt,
    Create,
    Keys,
    Set,
    Parse,
    Stringify,
    Atob,
    Btoa,
    StorageGet(StorageKind),
    StorageSet(StorageKind),
    StorageRemove(StorageKind),
    StorageClear(StorageKind),
    StorageKey(StorageKind),
    DocumentAttribute,
    DocumentQuery,
    CreateElement,
    MeasureLayout,
    EstimateStorage,
    CanvasContext,
    CanvasSerialize,
    StringSplit,
    ArrayPop,
    BodyAppendChild,
    BodyRemoveChild,
    ElementRemove,
    ElementRect,
    RectToJson,
    AddEventListener,
    RemoveEventListener,
    Bind,
    Construct,
    WeakSetAdd,
    WeakSetHas,
    WeakSetDelete,
    IsFinite,
    IsNan,
    NumberIsFinite,
    NumberIsNan,
    NumberIsInteger,
    NumberIsSafeInteger,
}

impl Host {
    fn tag(self) -> &'static str {
        match self {
            Self::Window => "Window",
            Self::Navigator => "Navigator",
            Self::Document => "HTMLDocument",
            Self::RouterContext => "Object",
            Self::DocumentElement => "HTMLHtmlElement",
            Self::Body => "HTMLBodyElement",
            Self::Dataset => "DOMStringMap",
            Self::Location => "Location",
            Self::Performance => "Performance",
            Self::Date => "Function",
            Self::Memory => "MemoryInfo",
            Self::Screen => "Screen",
            Self::ScreenOrientation => "ScreenOrientation",
            Self::History => "History",
            Self::Math => "Math",
            Self::Object => "Function",
            Self::Reflect => "Reflect",
            Self::Json => "JSON",
            Self::Storage(_) => "Storage",
            Self::StorageManager => "StorageManager",
            Self::WeakSetConstructor => "Function",
            Self::WeakSetPrototype => "WeakSet",
            Self::Number => "Function",
        }
    }
}

impl Builtin {
    fn name(self) -> &'static str {
        match self {
            Self::Random => "random",
            Self::Now => "now",
            Self::EpochNow => "now",
            Self::Hypot => "hypot",
            Self::Sqrt => "sqrt",
            Self::Create => "create",
            Self::Keys => "keys",
            Self::Set => "set",
            Self::Parse => "parse",
            Self::Stringify => "stringify",
            Self::Atob => "atob",
            Self::Btoa => "btoa",
            Self::StorageGet(_) => "getItem",
            Self::StorageSet(_) => "setItem",
            Self::StorageRemove(_) => "removeItem",
            Self::StorageClear(_) => "clear",
            Self::StorageKey(_) => "key",
            Self::DocumentAttribute => "getAttribute",
            Self::DocumentQuery => "querySelector",
            Self::CreateElement => "createElement",
            Self::MeasureLayout => "getBoundingClientRect",
            Self::EstimateStorage => "estimate",
            Self::CanvasContext => "getContext",
            Self::CanvasSerialize => "toDataURL",
            Self::StringSplit => "split",
            Self::ArrayPop => "pop",
            Self::BodyAppendChild => "appendChild",
            Self::BodyRemoveChild => "removeChild",
            Self::ElementRemove => "remove",
            Self::ElementRect => "getBoundingClientRect",
            Self::RectToJson => "toJSON",
            Self::AddEventListener => "addEventListener",
            Self::RemoveEventListener => "removeEventListener",
            Self::Bind => "bind",
            Self::Construct => "construct",
            Self::WeakSetAdd => "add",
            Self::WeakSetHas => "has",
            Self::WeakSetDelete => "delete",
            Self::IsFinite | Self::NumberIsFinite => "isFinite",
            Self::IsNan | Self::NumberIsNan => "isNaN",
            Self::NumberIsInteger => "isInteger",
            Self::NumberIsSafeInteger => "isSafeInteger",
        }
    }
}

struct Vm<P> {
    budget: Budget,
    profile: P,
    registers: BTreeMap<String, Value>,
    heap: Vec<Heap>,
    router_fields: Option<BTreeMap<String, Value>>,
    result: Option<String>,
    exception: Option<GuestException>,
    body_children: Vec<Value>,
    layout: NativeLayout,
    listeners: Vec<EventListener>,
    observer_globals: Option<BTreeMap<&'static str, Value>>,
    window_handlers: BTreeMap<&'static str, Value>,
}

const WINDOW_HANDLERS: &[&str] = &["oninput", "onbeforeinput"];

// Collector-owned fields, never imported from a browser or populated with
// synthetic input. Each observer window starts with no registered handlers;
// only its bytecode may install handlers/counters for its later snapshot.
const OBSERVER_GLOBALS: &[&str] = &[
    "__oai_so_bc",
    "__oai_so_bm",
    "__oai_so_cn",
    "__oai_so_cs",
    "__oai_so_cs2",
    "__oai_so_fn",
    "__oai_so_fs",
    "__oai_so_fs2",
    "__oai_so_h",
    "__oai_so_hc",
    "__oai_so_hi",
    "__oai_so_hp",
    "__oai_so_hpm",
    "__oai_so_ht",
    "__oai_so_hw",
    "__oai_so_i",
    "__oai_so_in",
    "__oai_so_ink",
    "__oai_so_k",
    "__oai_so_kp",
    "__oai_so_lx",
    "__oai_so_ly",
    "__oai_so_m",
    "__oai_so_owner",
    "__oai_so_p",
    "__oai_so_pc",
    "__oai_so_pm",
    "__oai_so_s",
    "__oai_so_sn",
    "__oai_so_sp",
    "__oai_so_spt",
    "__oai_so_ss",
    "__oai_so_ss2",
    "__oai_so_st",
    "__oai_so_sw",
    "__oai_so_sx0",
    "__oai_so_sy0",
    "__oai_so_t0",
    "__oai_so_ui",
    "__oai_so_uin",
    "__oai_so_uk",
    "__oai_so_wb",
    "__oai_so_wd",
    "__oai_so_we",
    "__oai_so_wl",
];

struct EventListener {
    target: Value,
    event_type: String,
    callback: Value,
    // Capture, once and passive are retained exactly as registered. This
    // native environment currently has no user-input or dispatchEvent source.
    options: u8,
}

struct GuestException {
    description: String,
    failure: Error,
}

pub(crate) fn solve(
    dx: &str,
    p: &str,
    profile: &Profile,
    cancellation: CancellationToken,
) -> Result<String> {
    solve_with_limits(dx, p, profile, cancellation, Limits::default())
}

fn solve_with_limits(
    dx: &str,
    p: &str,
    profile: &Profile,
    cancellation: CancellationToken,
    limits: Limits,
) -> Result<String> {
    Vm::new(profile, p, cancellation, limits)?.execute_encoded(dx)
}

/// One observer window, owned by the native preparation attempt. Collector and
/// snapshot share registers, heap and a cumulative budget; no global VM exists.
pub(crate) struct Observer {
    vm: Vm<Arc<Profile>>,
}

impl Observer {
    pub(crate) fn start(
        collector: &str,
        p: &str,
        profile: Arc<Profile>,
        cancellation: CancellationToken,
    ) -> Result<Self> {
        let mut vm = Vm::new(profile, p, cancellation, Limits::default())?;
        vm.enable_observer_globals()?;
        vm.execute_encoded(collector)?;
        vm.budget.pause()?;
        Ok(Self { vm })
    }

    pub(crate) fn snapshot(mut self, snapshot: &str) -> Result<String> {
        self.vm.budget.resume()?;
        self.vm.execute_encoded(snapshot)
    }
}

impl<P: Deref<Target = Profile>> Vm<P> {
    fn new(profile: P, p: &str, cancellation: CancellationToken, limits: Limits) -> Result<Self> {
        let mut vm = Self {
            budget: Budget::new(limits, cancellation),
            profile,
            registers: BTreeMap::new(),
            heap: Vec::new(),
            router_fields: None,
            result: None,
            exception: None,
            body_children: Vec::new(),
            layout: NativeLayout::default(),
            listeners: Vec::new(),
            observer_globals: None,
            window_handlers: BTreeMap::new(),
        };
        vm.budget.check()?;
        if p.is_empty() || p.len() > 16 * 1024 {
            return Err(Error::InputLimit);
        }
        // Instructions are callable register values. Programs may copy or
        // overwrite them; each observer window gets its own initial table.
        for op in [
            1, 2, 3, 4, 5, 6, 7, 8, 11, 12, 13, 14, 15, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
            28, 29, 30, 33, 34, 35,
        ] {
            vm.registers
                .insert(numeric_key(f64::from(op)), Value::Opcode(op));
        }
        vm.registers
            .insert(numeric_key(10.0), Value::Host(Host::Window));
        let p = vm.text(p.to_owned())?;
        vm.registers.insert(numeric_key(16.0), p);
        Ok(vm)
    }

    fn enable_observer_globals(&mut self) -> Result<()> {
        self.budget.allocate(OBSERVER_GLOBALS.len() * 128)?;
        self.observer_globals = Some(
            OBSERVER_GLOBALS
                .iter()
                .map(|name| (*name, Value::Undefined))
                .collect(),
        );
        Ok(())
    }

    fn execute_encoded(&mut self, dx: &str) -> Result<String> {
        self.budget.check()?;
        if dx.len() > self.budget.limits.input_bytes {
            return Err(Error::InputLimit);
        }
        // Reserve parser/UTF-16 scratch before decoding; cumulative across the
        // collector and snapshot, including a program that mutates its key.
        self.budget.allocate(dx.len().saturating_mul(32))?;
        let decoded = BASE64_STANDARD
            .decode(dx)
            .map_err(|_| Error::InvalidProgramEncoding)?;
        let decoded: String = decoded.into_iter().map(char::from).collect();
        let key = self.string(self.get(Value::Number(16.0))?, 0)?;
        let plain = xor(&decoded, &key)?;
        let program: Json = serde_json::from_str(&plain).map_err(|_| Error::InvalidProgramJson)?;
        if !program.is_array() {
            return Err(Error::InvalidProgramShape);
        }
        self.result = None;
        self.exception = None;
        // The official entrypoint installs fresh resolve/reject/callback
        // functions while preserving all other observer state.
        for op in [3, 4, 30] {
            self.registers
                .insert(numeric_key(f64::from(op)), Value::Opcode(op));
        }
        let program = self.import(program, 0)?;
        self.registers.insert(numeric_key(9.0), program);
        if let Err(failure) = self.run(0) {
            return Err(if failure == Error::GuestException {
                self.exception
                    .take()
                    .map_or(Error::MalformedProgram, |e| e.failure)
            } else {
                failure
            });
        }
        self.result
            .take()
            .filter(|s| !s.is_empty())
            .ok_or(Error::MissingResult)
    }

    fn catch_guest(&mut self, result: Result<Value>) -> Result<Value> {
        match result {
            Err(Error::GuestException) => {
                let exception = self.exception.take().ok_or(Error::MalformedProgram)?;
                self.text(exception.description)
            }
            result => result,
        }
    }

    fn alloc(&mut self, item: Heap, size: usize) -> Result<Value> {
        // Charge the arena slot and its possible Vec growth as well as the
        // payload. Diagnostic enums must not silently inflate guest values.
        self.budget
            .allocate(size.saturating_add(2 * size_of::<Heap>()))?;
        let id = self.heap.len();
        self.heap.push(item);
        Ok(Value::Heap(id))
    }

    fn text(&mut self, text: String) -> Result<Value> {
        let len = text.len();
        self.alloc(Heap::Text(text), len)
    }

    fn array(&mut self, items: Vec<Value>) -> Result<Value> {
        let len = items.len().saturating_mul(size_of::<Value>());
        self.alloc(Heap::Array(items), len)
    }

    fn import(&mut self, input: Json, depth: usize) -> Result<Value> {
        self.budget.step(depth)?;
        Ok(match input {
            Json::Null => Value::Null,
            Json::Bool(v) => Value::Bool(v),
            Json::Number(v) => Value::Number(v.as_f64().ok_or(Error::MalformedProgram)?),
            Json::String(v) => self.text(v)?,
            Json::Array(v) => {
                let items = v
                    .into_iter()
                    .map(|v| self.import(v, depth + 1))
                    .collect::<Result<Vec<_>>>()?;
                self.array(items)?
            }
            Json::Object(v) => {
                let mut fields = Vec::new();
                for (k, v) in v {
                    self.budget.allocate(k.len() + 32)?;
                    fields.push((k, self.import(v, depth + 1)?));
                }
                self.alloc(Heap::Object(fields), 0)?
            }
        })
    }

    fn register_key(&self, key: Value) -> Result<String> {
        match key.required()? {
            Value::Number(n) if n.is_finite() => Ok(numeric_key(n)),
            Value::Heap(id) => match &self.heap[id] {
                Heap::Text(t) if t.len() <= 128 => Ok(format!("s:{t}")),
                _ => Err(Error::InvalidRegister),
            },
            _ => Err(Error::InvalidRegister),
        }
    }

    fn get(&self, key: Value) -> Result<Value> {
        Ok(self
            .registers
            .get(&self.register_key(key)?)
            .copied()
            .unwrap_or(Value::Undefined))
    }

    fn set(&mut self, key: Value, value: Value) -> Result<()> {
        let key = self.register_key(key)?;
        if !self.registers.contains_key(&key) {
            self.budget.allocate(key.len() + 64)?;
        }
        self.registers.insert(key, value);
        Ok(())
    }

    fn items(&mut self, value: Value) -> Result<Vec<Value>> {
        if let Value::Heap(id) = value.required()?
            && let Heap::Array(items) = &self.heap[id]
        {
            self.budget
                .allocate(items.len().saturating_mul(size_of::<Value>()))?;
            return Ok(items.clone());
        }
        Err(Error::InvalidArray)
    }

    fn run(&mut self, depth: usize) -> Result<()> {
        loop {
            self.budget.step(depth)?;
            let queue = self
                .registers
                .get(&numeric_key(9.0))
                .copied()
                .ok_or(Error::InvalidQueue)?;
            let Value::Heap(id) = queue else {
                return Err(Error::InvalidQueue);
            };
            let Heap::Array(queue) = &mut self.heap[id] else {
                return Err(Error::InvalidQueue);
            };
            if queue.is_empty() {
                return Ok(());
            }
            let instruction = queue.remove(0);
            let instruction = self.items(instruction)?;
            let (key, args) = instruction.split_first().ok_or(Error::InvalidArguments)?;
            let callable = self.get(*key)?;
            self.call(callable, args, depth + 1)?;
        }
    }

    fn dereference(&self, args: &[Value]) -> Result<Vec<Value>> {
        args.iter().map(|a| self.get(*a)).collect()
    }

    fn call(&mut self, callable: Value, args: &[Value], depth: usize) -> Result<Value> {
        self.budget.step(depth)?;
        if matches!(callable, Value::AbsentRouterContext) {
            return Err(Error::RequiredRouterContextCall);
        }
        match callable.required()? {
            Value::Opcode(op) => self.instruction(op, args, depth),
            Value::Builtin(f) => self.builtin(f, args, depth),
            Value::Host(Host::Number) => Ok(Value::Number(match args.first() {
                Some(value) => self.number(*value)?,
                None => 0.0,
            })),
            Value::Host(Host::WeakSetConstructor) => {
                self.exception = Some(GuestException {
                    description: "TypeError: Constructor WeakSet requires 'new'".into(),
                    failure: Error::UnsupportedReflectionApi,
                });
                Err(Error::GuestException)
            }
            Value::Host(Host::Date) => self.text(
                chrono::Utc::now()
                    .format("%a %b %d %Y %H:%M:%S GMT+0000 (Coordinated Universal Time)")
                    .to_string(),
            ),
            Value::Host(Host::Object) => match args
                .first()
                .copied()
                .unwrap_or(Value::Undefined)
                .required()?
            {
                Value::Undefined | Value::Null => self.alloc(Heap::Object(Vec::new()), 0),
                Value::Heap(id) if !matches!(self.heap[id], Heap::Text(_)) => Ok(Value::Heap(id)),
                _ => Err(Error::UnsupportedObjectConversion),
            },
            Value::Heap(id) => {
                if let Heap::Bound {
                    function,
                    receiver,
                    arguments,
                } = &self.heap[id]
                {
                    let (function, receiver) = (*function, *receiver);
                    let combined = if arguments.is_empty() {
                        None
                    } else {
                        self.budget.allocate(
                            arguments.len().saturating_add(args.len()) * size_of::<Value>(),
                        )?;
                        Some(arguments.iter().chain(args).copied().collect::<Vec<_>>())
                    };
                    let args = combined.as_deref().unwrap_or(args);
                    if let Value::Builtin(
                        method @ (Builtin::WeakSetAdd
                        | Builtin::WeakSetHas
                        | Builtin::WeakSetDelete),
                    ) = function
                    {
                        return self.weak_set(
                            method,
                            receiver,
                            args.first().copied().unwrap_or(Value::Undefined),
                            depth + 1,
                        );
                    }
                    if matches!(function, Value::Builtin(Builtin::Bind)) {
                        if !self.is_callable(receiver) {
                            return Err(Error::UnsupportedNonCallableBinding);
                        }
                        let prefix = args.get(1..).unwrap_or_default();
                        self.budget.allocate(size_of_val(prefix))?;
                        return self.alloc(
                            Heap::Bound {
                                function: receiver,
                                receiver: args.first().copied().unwrap_or(Value::Undefined),
                                arguments: prefix.to_vec(),
                            },
                            0,
                        );
                    }
                    if let Value::Builtin(
                        method @ (Builtin::AddEventListener | Builtin::RemoveEventListener),
                    ) = function
                    {
                        return self.event_listener(method, receiver, args, depth + 1);
                    }
                    if matches!(function, Value::Builtin(Builtin::ElementRect)) {
                        return self.element_rect(receiver);
                    }
                    if matches!(function, Value::Builtin(Builtin::RectToJson)) {
                        let Value::Heap(id) = receiver else {
                            return Err(Error::UnsupportedDomLayout);
                        };
                        let Heap::Rect(rect) = self.heap[id] else {
                            return Err(Error::UnsupportedDomLayout);
                        };
                        let fields = rect_fields(rect)
                            .into_iter()
                            .map(|(key, value)| (key.to_owned(), Value::Number(value)))
                            .collect();
                        return self.alloc(Heap::Object(fields), 8 * 64);
                    }
                    if let Value::Builtin(
                        method @ (Builtin::BodyAppendChild
                        | Builtin::BodyRemoveChild
                        | Builtin::ElementRemove),
                    ) = function
                    {
                        return self.mutate_body(method, receiver, args, depth + 1);
                    }
                    if matches!(function, Value::Builtin(Builtin::StringSplit)) {
                        return self.split_string(receiver, args, depth + 1);
                    }
                    if matches!(function, Value::Builtin(Builtin::ArrayPop)) {
                        let Value::Heap(id) = receiver else {
                            return Err(Error::UnsupportedArrayApi);
                        };
                        let Heap::Array(items) = &mut self.heap[id] else {
                            return Err(Error::UnsupportedArrayApi);
                        };
                        return Ok(items.pop().unwrap_or(Value::Undefined));
                    }
                    return self.call(function, args, depth + 1);
                }
                let Heap::Callback {
                    result,
                    args: mapped,
                    program,
                } = &self.heap[id]
                else {
                    return Err(Error::UnknownInstruction);
                };
                let (result, program) = (*result, *program);
                self.budget
                    .allocate(mapped.len().saturating_mul(size_of::<Value>()))?;
                let mapped = mapped.clone();
                for (i, key) in mapped.into_iter().enumerate() {
                    self.set(key, args.get(i).copied().unwrap_or(Value::Undefined))?;
                }
                let saved = self
                    .registers
                    .get(&numeric_key(9.0))
                    .copied()
                    .ok_or(Error::InvalidQueue)?;
                let items = self.items(program)?;
                let queue = self.array(items)?;
                self.registers.insert(numeric_key(9.0), queue);
                let outcome = self.run(depth + 1);
                self.registers.insert(numeric_key(9.0), saved);
                outcome?;
                self.get(result)
            }
            _ => Err(Error::UnknownInstruction),
        }
    }

    fn instruction(&mut self, op: u8, args: &[Value], depth: usize) -> Result<Value> {
        let a = |i: usize| args.get(i).copied().ok_or(Error::InvalidArguments);
        match op {
            1 => {
                let left = self.string(self.get(a(0)?)?, depth)?;
                let right = self.string(self.get(a(1)?)?, depth)?;
                self.budget.allocate(left.len().saturating_mul(2))?;
                let result = self.text(xor(&left, &right)?)?;
                self.set(a(0)?, result)?;
            }
            2 => self.set(a(0)?, a(1)?)?,
            3 if self.result.is_none() => {
                let text = self.string(a(0)?, depth)?;
                self.budget.allocate(text.len().saturating_mul(2))?;
                self.result = Some(btoa(&text)?);
            }
            4 if self.result.is_none() => return Err(Error::Rejected),
            3 | 4 | 25 | 26 | 28 => {}
            5 => {
                let left = self.get(a(0)?)?;
                let right = self.get(a(1)?)?;
                if let Value::Heap(id) = left
                    && let Heap::Array(items) = &mut self.heap[id]
                {
                    self.budget.allocate(size_of::<Value>())?;
                    items.push(right);
                } else {
                    let result = if self.is_text(left) || self.is_text(right) {
                        let l = self.string(left, depth)?;
                        let r = self.string(right, depth)?;
                        self.text(format!("{l}{r}"))?
                    } else {
                        Value::Number(self.number(left)? + self.number(right)?)
                    };
                    self.set(a(0)?, result)?;
                }
            }
            6 | 24 => {
                let object = self.get(a(1)?)?;
                let key = self.get(a(2)?)?;
                let value = self.property(object, key, depth)?;
                let value = if op == 24 {
                    let value = value.required()?;
                    if !self.is_callable(value) {
                        return Err(Error::UnsupportedNonCallableBinding);
                    }
                    self.alloc(
                        Heap::Bound {
                            function: value,
                            receiver: self.get(a(1)?)?,
                            arguments: Vec::new(),
                        },
                        2 * size_of::<Value>(),
                    )?
                } else {
                    value
                };
                self.set(a(0)?, value)?;
            }
            7 => {
                self.call(self.get(a(0)?)?, &self.dereference(&args[1..])?, depth + 1)?;
            }
            8 | 34 => self.set(a(0)?, self.get(a(1)?)?)?,
            11 => {
                if args.len() != 2 {
                    return Err(Error::InvalidArguments);
                }
                let pattern = self.string(self.get(a(1)?)?, depth)?;
                let found = super::pattern::find(
                    &pattern,
                    &self.profile.bootstrap.scripts,
                    &mut self.budget,
                )?;
                let found = match found {
                    Some(found) => self.text(found)?,
                    // Verified against the official runtime with synthetic
                    // programs; an exhausted search returns null, not undefined.
                    None => Value::Null,
                };
                self.set(a(0)?, found)?;
            }
            12 => self.set(a(0)?, Value::Registers)?,
            13 => {
                let outcome = self.call(self.get(a(1)?)?, &args[2..], depth + 1);
                if outcome.is_err() {
                    let value = self.catch_guest(outcome)?;
                    self.set(a(0)?, value)?;
                }
            }
            14 => {
                let text = self.string(self.get(a(1)?)?, depth)?;
                if text.len() > self.budget.limits.input_bytes {
                    return Err(Error::InputLimit);
                }
                self.budget.allocate(text.len().saturating_mul(32))?;
                let json = serde_json::from_str(&text).map_err(|_| Error::InvalidJson)?;
                let value = self.import(json, depth + 1)?;
                self.set(a(0)?, value)?;
            }
            15 => {
                let result = self.stringify(self.get(a(1)?)?, depth + 1)?;
                self.set(a(0)?, result)?;
            }
            17 => {
                let callable = self.get(a(1)?)?;
                let arguments = self.dereference(&args[2..])?;
                let result = if self.is_estimate(callable, depth)? {
                    // The native call-and-set contract consumes the resolved
                    // host result. Only this ready, bounded storage operation
                    // is implemented; general Promise execution is unavailable.
                    let (usage, quota) = self
                        .profile
                        .storage(StorageKind::Local)
                        .lock()
                        .map_err(|_| Error::UnsupportedStorageEstimate)?
                        .estimate();
                    self.import(serde_json::json!({"usage":usage,"quota":quota}), depth + 1)
                } else {
                    self.call(callable, &arguments, depth + 1)
                };
                let result = self.catch_guest(result)?;
                self.set(a(0)?, result)?;
            }
            18 | 19 => {
                let text = self.string(self.get(a(0)?)?, depth)?;
                self.budget.allocate(text.len().saturating_mul(2))?;
                let text = if op == 18 { atob(&text)? } else { btoa(&text)? };
                let result = self.text(text)?;
                self.set(a(0)?, result)?;
            }
            20 => {
                if self.equal(self.get(a(0)?)?, self.get(a(1)?)?)? {
                    self.call(self.get(a(2)?)?, &args[3..], depth + 1)?;
                }
            }
            21 => {
                let l = self.number(self.get(a(0)?)?)?;
                let r = self.number(self.get(a(1)?)?)?;
                let threshold = self.number(self.get(a(2)?)?)?;
                if (l - r).abs() > threshold {
                    self.call(self.get(a(3)?)?, &args[4..], depth + 1)?;
                }
            }
            22 => {
                let saved = self
                    .registers
                    .get(&numeric_key(9.0))
                    .copied()
                    .ok_or(Error::InvalidQueue)?;
                let program = self.items(a(1)?)?;
                let program = self.array(program)?;
                self.registers.insert(numeric_key(9.0), program);
                let outcome = self.run(depth + 1);
                self.registers.insert(numeric_key(9.0), saved);
                if let Err(failure) = outcome {
                    let value = self.catch_guest(Err(failure))?;
                    self.set(a(0)?, value)?;
                }
            }
            23 => {
                if !matches!(
                    self.get(a(0)?)?,
                    Value::Undefined | Value::AbsentRouterContext
                ) {
                    self.call(self.get(a(1)?)?, &args[2..], depth + 1)?;
                }
            }
            27 => {
                let l = self.get(a(0)?)?;
                let r = self.get(a(1)?)?;
                let result = if let Value::Heap(id) = l
                    && matches!(self.heap[id], Heap::Array(_))
                {
                    let items = self.items(l)?;
                    let mut index = None;
                    for (i, item) in items.iter().enumerate() {
                        if self.equal(*item, r)? {
                            index = Some(i);
                            break;
                        }
                    }
                    // indexOf followed by splice: no match removes the last
                    // entry, and aliases observe the same array mutation.
                    if let Heap::Array(items) = &mut self.heap[id]
                        && !items.is_empty()
                    {
                        items.remove(index.unwrap_or(items.len() - 1));
                    }
                    l
                } else {
                    Value::Number(self.number(l)? - self.number(r)?)
                };
                self.set(a(0)?, result)?;
            }
            29 => {
                let l = self.get(a(1)?)?.required()?;
                let r = self.get(a(2)?)?.required()?;
                let less = if let (Value::Heap(left), Value::Heap(right)) = (l, r)
                    && let (Heap::Text(left), Heap::Text(right)) =
                        (&self.heap[left], &self.heap[right])
                {
                    self.budget.work(left.len() + right.len(), depth)?;
                    left.encode_utf16().cmp(right.encode_utf16()).is_lt()
                } else {
                    self.number(l)? < self.number(r)?
                };
                self.set(a(0)?, Value::Bool(less))?;
            }
            33 | 35 => {
                let l = self.number(self.get(a(1)?)?)?;
                let r = self.number(self.get(a(2)?)?)?;
                let result = match op {
                    33 => Value::Number(l * r),
                    _ => Value::Number(if r == 0.0 { 0.0 } else { l / r }),
                };
                self.set(a(0)?, result)?;
            }
            30 => {
                let (mapped, program) = if args.len() == 3 {
                    (Vec::new(), a(2)?)
                } else if args.len() == 4 {
                    (self.items(a(2)?)?, a(3)?)
                } else {
                    return Err(Error::InvalidArguments);
                };
                let callback = self.alloc(
                    Heap::Callback {
                        result: a(1)?,
                        args: mapped,
                        program,
                    },
                    std::mem::size_of_val(args),
                )?;
                self.set(a(0)?, callback)?;
            }
            _ => return Err(Error::UnknownInstruction),
        }
        Ok(Value::Undefined)
    }

    fn is_text(&self, v: Value) -> bool {
        matches!(v, Value::Heap(id) if matches!(self.heap[id], Heap::Text(_)))
    }

    fn is_callable(&self, value: Value) -> bool {
        matches!(
            value,
            Value::Opcode(_)
                | Value::Builtin(_)
                | Value::Host(Host::Object | Host::Date | Host::WeakSetConstructor | Host::Number)
        ) || matches!(value, Value::Heap(id) if matches!(self.heap[id], Heap::Callback {..} | Heap::Bound { .. }))
    }

    fn is_estimate(&mut self, mut value: Value, mut depth: usize) -> Result<bool> {
        loop {
            self.budget.step(depth)?;
            match value {
                Value::Heap(id) => match self.heap[id] {
                    Heap::Bound {
                        function: target, ..
                    } => {
                        value = target;
                        depth += 1;
                    }
                    _ => return Ok(false),
                },
                Value::Builtin(Builtin::EstimateStorage) => return Ok(true),
                _ => return Ok(false),
            }
        }
    }

    fn number(&self, v: Value) -> Result<f64> {
        Ok(match v.required()? {
            Value::Number(n) => n,
            Value::Null => 0.0,
            Value::Undefined => f64::NAN,
            Value::Bool(b) => {
                if b {
                    1.0
                } else {
                    0.0
                }
            }
            Value::Heap(id) => match &self.heap[id] {
                Heap::Text(t) => numeric_string(t)?,
                _ => return Err(Error::UnsupportedNumericCoercion),
            },
            _ => return Err(Error::UnsupportedNumericCoercion),
        })
    }

    fn equal(&self, a: Value, b: Value) -> Result<bool> {
        Ok(match (a, b) {
            (Value::AbsentRouterContext, Value::Undefined | Value::AbsentRouterContext)
            | (Value::Undefined, Value::AbsentRouterContext) => true,
            (Value::Undefined, Value::Undefined) | (Value::Null, Value::Null) => true,
            (Value::Number(a), Value::Number(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Host(a), Value::Host(b)) => a == b,
            (Value::Builtin(a), Value::Builtin(b)) => a == b,
            (Value::Opcode(a), Value::Opcode(b)) => a == b,
            (Value::Registers, Value::Registers) => true,
            (Value::Heap(a), Value::Heap(b)) => match (&self.heap[a], &self.heap[b]) {
                (Heap::Text(a), Heap::Text(b)) => a == b,
                _ => a == b,
            },
            _ => false,
        })
    }

    fn string(&mut self, value: Value, depth: usize) -> Result<String> {
        self.budget.step(depth)?;
        let text = match value {
            Value::AbsentRouterContext => {
                return Err(Error::RequiredRouterContextCoercion);
            }
            Value::Undefined => "undefined".into(),
            Value::Null => "null".into(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => number_string(n)?,
            Value::Heap(id) => match &self.heap[id] {
                Heap::Text(t) => {
                    self.budget.allocate(t.len())?;
                    return Ok(t.clone());
                }
                Heap::Object(_) => "[object Object]".into(),
                Heap::NullObject(_) => return Err(Error::UnsupportedNullObjectCoercion),
                Heap::WeakSet(_) => "[object WeakSet]".into(),
                Heap::Canvas { .. } => "[object HTMLCanvasElement]".into(),
                Heap::Rect(_) => "[object DOMRect]".into(),
                Heap::Element { kind, .. } => format!("[object {}]", kind.interface()),
                Heap::Style(_) => "[object CSSStyleDeclaration]".into(),
                Heap::Bound { .. } => "function () { [native code] }".into(),
                Heap::Callback { .. } => return Err(Error::UnsupportedCallbackSource),
                Heap::Array(_) => {
                    let items = self.items(value)?;
                    let mut out = String::new();
                    for (i, item) in items.into_iter().enumerate() {
                        if i != 0 {
                            self.budget.allocate(1)?;
                            out.push(',');
                        }
                        if !matches!(item, Value::Undefined | Value::Null) {
                            let item = self.string(item, depth + 1)?;
                            self.budget.allocate(item.len())?;
                            out.push_str(&item);
                        }
                    }
                    out
                }
            },
            Value::Host(Host::Math) => "[object Math]".into(),
            Value::Host(Host::Performance) => "[object Performance]".into(),
            Value::Builtin(function) => {
                format!("function {}() {{ [native code] }}", function.name())
            }
            Value::Opcode(_) => return Err(Error::UnsupportedInterpreterSource),
            Value::Registers => "[object Map]".into(),
            Value::Host(host) => match host {
                Host::Location => format!("{}/", self.profile.bootstrap.origin),
                Host::Object => "function Object() { [native code] }".into(),
                Host::Date => "function Date() { [native code] }".into(),
                Host::WeakSetConstructor => "function WeakSet() { [native code] }".into(),
                Host::Number => "function Number() { [native code] }".into(),
                host => format!("[object {}]", host.tag()),
            },
        };
        self.budget.allocate(text.len())?;
        Ok(text)
    }

    fn json(&mut self, value: Value, depth: usize) -> Result<Json> {
        self.budget.step(depth)?;
        if self.is_callable(value) {
            return Ok(Json::Null);
        }
        self.budget.allocate(64)?;
        Ok(match value {
            Value::AbsentRouterContext => {
                return Err(Error::RequiredRouterContextSerialization);
            }
            Value::Null | Value::Undefined | Value::Opcode(_) | Value::Builtin(_) => Json::Null,
            Value::Bool(b) => Json::Bool(b),
            Value::Number(n) => {
                if !n.is_finite() {
                    Json::Null
                } else {
                    serde_json::from_str(&number_string(n)?).map_err(|_| Error::MalformedProgram)?
                }
            }
            Value::Heap(id) => match &self.heap[id] {
                Heap::Text(t) => {
                    self.budget.allocate(t.len().saturating_mul(6))?;
                    Json::String(t.clone())
                }
                Heap::Array(_) => {
                    let items = self.items(value)?;
                    Json::Array(
                        items
                            .into_iter()
                            .map(|v| self.json(v, depth + 1))
                            .collect::<Result<_>>()?,
                    )
                }
                Heap::Object(fields) | Heap::NullObject(fields) => {
                    self.budget
                        .allocate(fields.iter().map(|(k, _)| k.len() + 32).sum())?;
                    let mut fields = fields.clone();
                    // JavaScript enumerates array-index keys before other keys.
                    fields.sort_by_key(|(k, _)| array_index(k).map_or((1, u32::MAX), |n| (0, n)));
                    let mut map = serde_json::Map::new();
                    for (k, v) in fields {
                        if !matches!(v, Value::Undefined) && !self.is_callable(v) {
                            map.insert(k, self.json(v, depth + 1)?);
                        }
                    }
                    Json::Object(map)
                }
                Heap::Canvas { .. } | Heap::WeakSet(_) => Json::Object(serde_json::Map::new()),
                Heap::Rect(rect) => {
                    let fields = rect_fields(*rect);
                    let mut out = serde_json::Map::new();
                    for (key, value) in fields {
                        self.budget.allocate(64)?;
                        out.insert(key.to_owned(), self.json(Value::Number(value), depth + 1)?);
                    }
                    Json::Object(out)
                }
                Heap::Element { .. } => Json::Object(serde_json::Map::new()),
                Heap::Style(_) => return Err(Error::UnsupportedStyleSerialization),
                Heap::Callback { .. } | Heap::Bound { .. } => {
                    return Err(Error::UnsupportedSerialization);
                }
            },
            _ => return Err(Error::UnsupportedSerialization),
        })
    }

    fn stringify(&mut self, value: Value, depth: usize) -> Result<Value> {
        if matches!(value, Value::Undefined) || self.is_callable(value) {
            return Ok(Value::Undefined);
        }
        let json = self.json(value, depth)?;
        self.text(serde_json::to_string(&json).map_err(|_| Error::UnsupportedSerialization)?)
    }

    fn property(&mut self, value: Value, key: Value, depth: usize) -> Result<Value> {
        let nullish = match value {
            Value::AbsentRouterContext => Some(("undefined", Error::RequiredRouterContextProperty)),
            Value::Undefined => Some(("undefined", Error::RequiredUndefinedProperty)),
            Value::Null => Some(("null", Error::RequiredNullProperty)),
            _ => None,
        };
        if let Some((kind, mut failure)) = nullish {
            let key = self.string(key, depth + 1)?;
            if matches!(value, Value::Null)
                && matches!(
                    key.as_str(),
                    "getExtension" | "getParameter" | "getImageData" | "fillText"
                )
            {
                failure = Error::UnsupportedCanvasContext;
            }
            self.budget.allocate(key.len().saturating_add(96))?;
            self.exception = Some(GuestException {
                description: format!(
                    "TypeError: Cannot read properties of {kind} (reading '{key}')"
                ),
                failure,
            });
            return Err(Error::GuestException);
        }
        let value = value.required()?;
        if matches!(value, Value::Registers) {
            return self.get(key);
        }
        let key = self.string(key, depth)?;
        let key = key.as_str();
        if key == "bind" && self.is_callable(value) {
            return Ok(Value::Builtin(Builtin::Bind));
        }
        match value {
            Value::Heap(id) => match &self.heap[id] {
                Heap::WeakSet(_) => self.weak_set_property(key),
                Heap::Element {
                    kind,
                    style,
                    id,
                    class_name,
                    text_content,
                    aria_hidden,
                    connected,
                    ..
                } => match key {
                    "tagName" | "nodeName" => self.text(kind.tag().into()),
                    "ownerDocument" => Ok(Value::Host(Host::Document)),
                    "nodeType" => Ok(Value::Number(1.0)),
                    "parentNode" | "parentElement" => Ok(if *connected {
                        Value::Host(Host::Body)
                    } else {
                        Value::Null
                    }),
                    "isConnected" => Ok(Value::Bool(*connected)),
                    "remove" => Ok(Value::Builtin(Builtin::ElementRemove)),
                    "addEventListener" => Ok(Value::Builtin(Builtin::AddEventListener)),
                    "removeEventListener" => Ok(Value::Builtin(Builtin::RemoveEventListener)),
                    "getBoundingClientRect" => Ok(Value::Builtin(Builtin::ElementRect)),
                    "id" => self.text(id.clone()),
                    "className" => self.text(class_name.clone()),
                    // Detached nodes expose descendant text. Attached innerText
                    // needs separate rendered-text semantics.
                    "textContent" => self.text(text_content.clone()),
                    "innerText" if !connected => self.text(text_content.clone()),
                    "innerText" => Err(Error::UnsupportedDomMeasurement {
                        element: kind.tag(),
                        property: "innerText",
                        connected: *connected,
                    }),
                    "ariaHidden" => match aria_hidden {
                        Some(value) => self.text(value.clone()),
                        None => Ok(Value::Null),
                    },
                    "style" => Ok(*style),
                    "clientWidth" | "clientHeight" | "offsetWidth" | "offsetHeight" => {
                        Err(Error::UnsupportedDomMeasurement {
                            element: kind.tag(),
                            property: diagnostic_dom_property(key),
                            connected: *connected,
                        })
                    }
                    _ => Err(Error::UnsupportedDomElement),
                },
                Heap::Rect(rect) => {
                    if key == "toJSON" {
                        return Ok(Value::Builtin(Builtin::RectToJson));
                    }
                    rect_fields(*rect)
                        .into_iter()
                        .find(|(name, _)| *name == key)
                        .map(|(_, value)| Value::Number(value))
                        .ok_or(Error::UnsupportedDomLayout)
                }
                Heap::Style(style) => {
                    if key == "length" {
                        return Ok(Value::Number(style.len() as f64));
                    }
                    let value = if key == "cssText" {
                        style.css_text(&mut self.budget)?
                    } else {
                        let value = style.get(key)?;
                        self.budget.allocate(value.len())?;
                        value.to_owned()
                    };
                    self.text(value)
                }
                Heap::Canvas { width, height } => match key {
                    "width" => Ok(Value::Number(f64::from(*width))),
                    "height" => Ok(Value::Number(f64::from(*height))),
                    "tagName" | "nodeName" => self.text("CANVAS".into()),
                    "getContext" => Ok(Value::Builtin(Builtin::CanvasContext)),
                    "toDataURL" => Ok(Value::Builtin(Builtin::CanvasSerialize)),
                    _ => Err(Error::UnsupportedCanvas),
                },
                Heap::Object(fields) => {
                    if let Some((_, value)) = fields.iter().find(|(k, _)| k == key) {
                        Ok(*value)
                    } else if inherited_property(key) {
                        Err(Error::UnsupportedPrototypeProperty {
                            value: "object",
                            property: diagnostic_js_property(key),
                        })
                    } else {
                        Ok(Value::Undefined)
                    }
                }
                Heap::NullObject(fields) => Ok(fields
                    .iter()
                    .find(|(k, _)| k == key)
                    .map_or(Value::Undefined, |(_, v)| *v)),
                Heap::Array(items) if key == "length" => Ok(Value::Number(items.len() as f64)),
                Heap::Array(_) if key == "pop" => Ok(Value::Builtin(Builtin::ArrayPop)),
                Heap::Array(_) if inherited_property(key) || array_method(key) => {
                    Err(Error::UnsupportedPrototypeProperty {
                        value: "array",
                        property: diagnostic_js_property(key),
                    })
                }
                Heap::Array(items) => Ok(array_index(key)
                    .and_then(|i| items.get(i as usize))
                    .copied()
                    .unwrap_or(Value::Undefined)),
                Heap::Text(t) if key == "length" => {
                    Ok(Value::Number(t.encode_utf16().count() as f64))
                }
                Heap::Text(_) if key == "split" => Ok(Value::Builtin(Builtin::StringSplit)),
                Heap::Text(_) if inherited_property(key) || string_method(key) => {
                    Err(Error::UnsupportedPrototypeProperty {
                        value: "string",
                        property: diagnostic_js_property(key),
                    })
                }
                Heap::Text(t) => {
                    let unit = array_index(key).and_then(|i| t.encode_utf16().nth(i as usize));
                    match unit {
                        Some(unit) => self.text(
                            String::from_utf16(&[unit]).map_err(|_| Error::UnsupportedString)?,
                        ),
                        None => Ok(Value::Undefined),
                    }
                }
                Heap::Bound { .. } | Heap::Callback { .. } => {
                    Err(Error::UnsupportedPrototypeProperty {
                        value: "function",
                        property: diagnostic_js_property(key),
                    })
                }
            },
            Value::Host(host) => self.host_property(host, key),
            Value::Opcode(_) => Err(Error::UnsupportedPrototypeProperty {
                value: "opcode_function",
                property: diagnostic_js_property(key),
            }),
            Value::Builtin(f) => Err(Error::UnsupportedPrototypeProperty {
                value: f.name(),
                property: diagnostic_js_property(key),
            }),
            Value::Bool(_) | Value::Number(_) => Err(Error::UnsupportedPrimitiveProperty),
            _ => Err(Error::UnsupportedPropertyTarget),
        }
    }

    fn weak_set_property(&self, key: &str) -> Result<Value> {
        Ok(match key {
            "add" => Value::Builtin(Builtin::WeakSetAdd),
            "has" => Value::Builtin(Builtin::WeakSetHas),
            "delete" => Value::Builtin(Builtin::WeakSetDelete),
            "constructor" => Value::Host(Host::WeakSetConstructor),
            "size" => Value::Undefined,
            _ => {
                return Err(Error::UnsupportedPrototypeProperty {
                    value: "WeakSet",
                    property: diagnostic_js_property(key),
                });
            }
        })
    }

    fn is_object(&self, value: Value) -> bool {
        self.is_callable(value)
            || matches!(value, Value::Host(_) | Value::Registers)
            || matches!(value, Value::Heap(id) if !matches!(self.heap[id], Heap::Text(_)))
    }

    fn weak_set(
        &mut self,
        method: Builtin,
        receiver: Value,
        key: Value,
        depth: usize,
    ) -> Result<Value> {
        self.budget.step(depth)?;
        let key = key.required()?;
        let Value::Heap(id) = receiver.required()? else {
            return Err(Error::UnsupportedReflectionApi);
        };
        let Heap::WeakSet(items) = &self.heap[id] else {
            return Err(Error::UnsupportedReflectionApi);
        };
        self.budget.work(items.len() + 1, depth)?;
        if !self.is_object(key) {
            if method == Builtin::WeakSetAdd {
                self.exception = Some(GuestException {
                    description: "TypeError: Invalid value used in weak set".into(),
                    failure: Error::UnsupportedReflectionApi,
                });
                return Err(Error::GuestException);
            }
            return Ok(Value::Bool(false));
        }
        let mut found = None;
        for (index, item) in items.iter().enumerate() {
            if self.equal(*item, key)? {
                found = Some(index);
                break;
            }
        }
        if method == Builtin::WeakSetAdd && found.is_none() {
            self.budget.allocate(2 * size_of::<Value>())?;
        }
        let Heap::WeakSet(items) = &mut self.heap[id] else {
            return Err(Error::MalformedProgram);
        };
        match method {
            Builtin::WeakSetAdd => {
                if found.is_none() {
                    items.push(key);
                }
                Ok(receiver)
            }
            Builtin::WeakSetHas => Ok(Value::Bool(found.is_some())),
            Builtin::WeakSetDelete => {
                if let Some(index) = found {
                    items.remove(index);
                }
                Ok(Value::Bool(found.is_some()))
            }
            _ => Err(Error::UnsupportedReflectionApi),
        }
    }

    fn host_property(&mut self, host: Host, key: &str) -> Result<Value> {
        use Builtin::*;
        use Host::*;
        Ok(match (host, key) {
            (Window, "undefined") => Value::Undefined,
            (Window, "NaN") => Value::Number(f64::NAN),
            (Window, "Infinity") => Value::Number(f64::INFINITY),
            (Window | Document | Body | DocumentElement, "addEventListener") => {
                Value::Builtin(AddEventListener)
            }
            (Window | Document | Body | DocumentElement, "removeEventListener") => {
                Value::Builtin(RemoveEventListener)
            }
            (Window | Document | Body | DocumentElement, "dispatchEvent") => {
                return Err(Error::UnsupportedEventTarget {
                    reason: "dispatch_source",
                });
            }
            (Window, "window" | "self" | "globalThis" | "frames" | "parent" | "top") => {
                Value::Host(Window)
            }
            (Window, "length") => Value::Number(0.0),
            (Window, "innerWidth" | "outerWidth") => Value::Number(f64::from(SCREEN_WIDTH)),
            (Window, "innerHeight" | "outerHeight") => Value::Number(f64::from(SCREEN_HEIGHT)),
            (
                Window,
                "screenX" | "screenY" | "screenLeft" | "screenTop" | "scrollX" | "scrollY"
                | "pageXOffset" | "pageYOffset",
            ) => Value::Number(0.0),
            (Window, "devicePixelRatio") => Value::Number(1.0),
            (Window, "isSecureContext") => {
                Value::Bool(self.profile.bootstrap.origin.starts_with("https://"))
            }
            (Window, "crossOriginIsolated") => Value::Bool(false),
            (Window, "event") => Value::Undefined,
            (Window, key) if WINDOW_HANDLERS.contains(&key) => self
                .window_handlers
                .get(key)
                .copied()
                .unwrap_or(Value::Null),
            (Window, "clientInformation") => Value::Host(Navigator),
            (Window, "__reactRouterContext") => match self.profile.bootstrap.router.as_ref() {
                Ok(_) => Value::Host(RouterContext),
                Err(Error::UnsupportedRouterContext) => Value::AbsentRouterContext,
                Err(error) => return Err(*error),
            },
            (RouterContext, "loaderData" | "state" | "stream" | "streamController") => {
                return Err(Error::UnsupportedRouterSessionState);
            }
            (RouterContext, key) => {
                if self.router_fields.is_none() {
                    let mut fields = BTreeMap::new();
                    // Router handoff parsing is capped at 16 KiB. Reserve its
                    // bounded clone before importing into this VM's arena.
                    self.budget.allocate(16 * 1024 * 8)?;
                    let observed = self
                        .profile
                        .bootstrap
                        .router
                        .as_ref()
                        .map_err(|e| *e)?
                        .clone();
                    for (key, value) in observed {
                        self.budget.allocate(key.len() + 64)?;
                        fields.insert(key, self.import(value, 1)?);
                    }
                    self.router_fields = Some(fields);
                }
                self.router_fields
                    .as_ref()
                    .and_then(|fields| fields.get(key))
                    .copied()
                    .ok_or(Error::UnsupportedRouterField)?
            }
            (Window, "__reactRouterManifest") => return Err(Error::UnsupportedRouterManifest),
            (Window, key) if OBSERVER_GLOBALS.contains(&key) => self
                .observer_globals
                .as_ref()
                .and_then(|fields| fields.get(key))
                .copied()
                .ok_or(Error::UnsupportedSentinelSdkState)?,
            (Window, "__reactRouterVersion") => return Err(Error::UnsupportedRouterVersion),
            (Window, "__reactRouterRouteModules") => return Err(Error::UnsupportedRouterModules),
            (Window, "__reactRouterDataRouter") => return Err(Error::UnsupportedRouterInstance),
            (Window, "__STATSIG__" | "DD_RUM") => return Err(Error::UnsupportedTelemetryState),
            (Window, "SentinelSDK" | "__sentinel_init_pending" | "__sentinel_token_pending") => {
                return Err(Error::UnsupportedSentinelSdkState);
            }
            (Window, key) if array_index(key).is_some() => Value::Undefined,
            (Window, "navigator") => Value::Host(Navigator),
            (Window, "document") => Value::Host(Document),
            (Window | Document, "location") => Value::Host(Location),
            (Window, "performance") => Value::Host(Performance),
            (Window, "screen") => Value::Host(Screen),
            (Window, "Date") => Value::Host(Date),
            (Date, "now") => Value::Builtin(EpochNow),
            (Date, "name") => self.text("Date".into())?,
            (Date, "length") => Value::Number(7.0),
            (Date, _) => return Err(Error::UnsupportedClockApi),
            (Window, "Array" | "Uint8Array" | "ArrayBuffer") => {
                return Err(Error::UnsupportedArrayApi);
            }
            (Window, "String" | "TextEncoder" | "TextDecoder") => {
                return Err(Error::UnsupportedStringApi);
            }
            (Window, "Number") => Value::Host(Number),
            (Window, "isFinite") => Value::Builtin(IsFinite),
            (Window, "isNaN") => Value::Builtin(IsNan),
            (Number, "name") => self.text("Number".into())?,
            (Number, "length") => Value::Number(1.0),
            (Number, "isFinite") => Value::Builtin(NumberIsFinite),
            (Number, "isNaN") => Value::Builtin(NumberIsNan),
            (Number, "isInteger") => Value::Builtin(NumberIsInteger),
            (Number, "isSafeInteger") => Value::Builtin(NumberIsSafeInteger),
            (Number, key) => {
                return Err(Error::UnsupportedBuiltinBinding {
                    object: "Number",
                    property: diagnostic_builtin_property(key),
                });
            }
            (Window, "BigInt" | "parseInt" | "parseFloat") => {
                return Err(Error::UnsupportedWindowBinding {
                    property: diagnostic_window_property(key),
                });
            }
            (Window, "Function" | "eval") => return Err(Error::UnsupportedFunctionApi),
            (Window, "Promise") => return Err(Error::UnsupportedPromiseApi),
            (
                Window,
                "setTimeout"
                | "clearTimeout"
                | "setInterval"
                | "clearInterval"
                | "requestAnimationFrame"
                | "requestIdleCallback",
            ) => return Err(Error::UnsupportedTimers),
            (Window, "crypto") => return Err(Error::UnsupportedCryptoApi),
            (Window, "chrome") => return Err(Error::UnsupportedChromeRuntime),
            (Window, "history") => Value::Host(History),
            (Window, "WeakSet") => Value::Host(WeakSetConstructor),
            (WeakSetConstructor, "name") => self.text("WeakSet".into())?,
            (WeakSetConstructor, "length") => Value::Number(0.0),
            (WeakSetConstructor, "prototype") => Value::Host(WeakSetPrototype),
            (WeakSetConstructor, _) => return Err(Error::UnsupportedReflectionApi),
            (WeakSetPrototype, "constructor") => Value::Host(WeakSetConstructor),
            (WeakSetPrototype, key) => return self.weak_set_property(key),
            (Window, "Proxy" | "Symbol" | "Map" | "Set" | "WeakMap") => {
                return Err(Error::UnsupportedWindowBinding {
                    property: diagnostic_window_property(key),
                });
            }
            (Window, "Math") => Value::Host(Math),
            (Window, "Object") => Value::Host(Object),
            (Window, "Reflect") => Value::Host(Reflect),
            (Window, "JSON") => Value::Host(Json),
            (Window, "atob") => Value::Builtin(Atob),
            (Window, "btoa") => Value::Builtin(Btoa),
            (Window, "getComputedStyle") => return Err(Error::UnsupportedDomLayout),
            (Window, "localStorage") => Value::Host(Storage(StorageKind::Local)),
            (Window, "sessionStorage") => Value::Host(Storage(StorageKind::Session)),
            (Storage(kind), "getItem") => Value::Builtin(StorageGet(kind)),
            (Storage(kind), "setItem") => Value::Builtin(StorageSet(kind)),
            (Storage(kind), "removeItem") => Value::Builtin(StorageRemove(kind)),
            (Storage(kind), "clear") => Value::Builtin(StorageClear(kind)),
            (Storage(kind), "key") => Value::Builtin(StorageKey(kind)),
            (Storage(kind), "length") => Value::Number(
                self.profile
                    .storage(kind)
                    .lock()
                    .map_err(|_| Error::UnsupportedStorage)?
                    .len() as f64,
            ),
            (Storage(kind), key) => {
                return Ok(self.storage_get(kind, key)?.unwrap_or(Value::Undefined));
            }
            (Navigator, "geolocation") => return Err(Error::UnsupportedGeolocation),
            (Navigator, "userAgentData") => return Err(Error::UnsupportedClientHints),
            (Navigator, "storage") => Value::Host(StorageManager),
            (StorageManager, "estimate") => Value::Builtin(EstimateStorage),
            (StorageManager, _) => return Err(Error::UnsupportedStorageApi),
            (Navigator, "plugins" | "mimeTypes" | "javaEnabled" | "pdfViewerEnabled") => {
                return Err(Error::UnsupportedPluginEnumeration);
            }
            (Navigator, "deviceMemory") => Value::Number(f64::from(DEVICE_MEMORY_GIB)),
            (Navigator, "clipboard") => return Err(Error::UnsupportedClipboard),
            (Navigator, "permissions") => return Err(Error::UnsupportedPermissions),
            (Navigator, "connection" | "onLine") => {
                return Err(Error::UnsupportedNetworkInformation);
            }
            // This in-process runtime has no WebDriver remote end or session.
            // Its WebDriver-active flag therefore stays at the defined false
            // initial state; this is not copied from the research browser.
            (Navigator, "webdriver") => Value::Bool(false),
            (Navigator, "cookieEnabled") => return Err(Error::UnsupportedCookieState),
            // HTML NavigatorID's Chrome compatibility mode, bound to the
            // transport user agent, rather than guessed per-request values.
            (Navigator, "appCodeName") => self.text("Mozilla".into())?,
            (Navigator, "appName") => self.text("Netscape".into())?,
            (Navigator, "appVersion") => self.text(
                USER_AGENT
                    .strip_prefix("Mozilla/")
                    .ok_or(Error::UnsupportedNavigatorProperty)?
                    .into(),
            )?,
            (Navigator, "product") => self.text("Gecko".into())?,
            (Navigator, "productSub") => self.text("20030107".into())?,
            (Navigator, "vendorSub") => self.text(String::new())?,
            (Navigator, "userAgent") => self.text(USER_AGENT.into())?,
            (Navigator, "platform") => self.text("Linux x86_64".into())?,
            (Navigator, "vendor") => self.text("Google Inc.".into())?,
            (Navigator, "language") => self.text("en-US".into())?,
            (Navigator, "languages") => {
                let en_us = self.text("en-US".into())?;
                let en = self.text("en".into())?;
                self.array(vec![en_us, en])?
            }
            (Navigator, "hardwareConcurrency") => Value::Number(f64::from(HARDWARE_CONCURRENCY)),
            (Navigator, "maxTouchPoints") => Value::Number(0.0),
            (Location, "href") => self.text(format!("{}/", self.profile.bootstrap.origin))?,
            (Location, "search") => self.text(String::new())?,
            (Document, "scripts") => {
                let mut scripts = Vec::new();
                for i in 0..self.profile.bootstrap.scripts.len() {
                    self.budget
                        .allocate(self.profile.bootstrap.scripts[i].len())?;
                    let script = self.profile.bootstrap.scripts[i].clone();
                    let src = self.text(script)?;
                    scripts.push(self.alloc(Heap::Object(vec![("src".into(), src)]), 32)?);
                }
                self.array(scripts)?
            }
            (Document, "documentElement") => Value::Host(DocumentElement),
            // An opaque node identity allows detecting the capability actually
            // used. No geometry, styling or rendered contents are invented.
            (Document, "body") => Value::Host(Body),
            (Body, "getBoundingClientRect") => Value::Builtin(MeasureLayout),
            (Body, "appendChild") => Value::Builtin(BodyAppendChild),
            (Body, "removeChild") => Value::Builtin(BodyRemoveChild),
            (Body, "clientWidth" | "clientHeight" | "offsetWidth" | "offsetHeight") => {
                return Err(Error::UnsupportedDomLayout);
            }
            (Body, _) => {
                return Err(Error::UnsupportedDomBodyProperty {
                    property: diagnostic_dom_property(key),
                });
            }
            (Document, "getElementById") => return Err(Error::UnsupportedDomQuery),
            (Document, "querySelector") => Value::Builtin(DocumentQuery),
            (Document, "createElement") => Value::Builtin(CreateElement),
            (Document, "cookie") => return Err(Error::UnsupportedDomCookies),
            (DocumentElement, "getAttribute") => Value::Builtin(DocumentAttribute),
            (DocumentElement, "dataset") => Value::Host(Dataset),
            (Dataset, "build") => self.text(self.profile.bootstrap.build.clone())?,
            (
                DocumentElement,
                "getBoundingClientRect"
                | "clientWidth"
                | "clientHeight"
                | "offsetWidth"
                | "offsetHeight",
            ) => return Err(Error::UnsupportedDomLayout),
            (DocumentElement | Dataset, _) => return Err(Error::UnsupportedDomAttribute),
            (Document, _) => return Err(Error::UnsupportedDom),
            (Performance, "now") => Value::Builtin(Now),
            (Performance, "timeOrigin") => Value::Number(self.profile.epoch_ms as f64),
            (Performance, "memory") => Value::Host(Memory),
            (Memory, "jsHeapSizeLimit") => Value::Number(JS_HEAP_LIMIT as f64),
            (Memory, _) => return Err(Error::UnsupportedMemoryInfo),
            (Screen, "width" | "availWidth") => Value::Number(f64::from(SCREEN_WIDTH)),
            (Screen, "height" | "availHeight") => Value::Number(f64::from(SCREEN_HEIGHT)),
            (Screen, "availTop" | "availLeft") => Value::Number(0.0),
            (Screen, "colorDepth" | "pixelDepth") => Value::Number(f64::from(SCREEN_COLOR_DEPTH)),
            (Screen, "isExtended") => Value::Bool(false),
            (Screen, "orientation") => Value::Host(ScreenOrientation),
            (ScreenOrientation, "angle") => Value::Number(0.0),
            (ScreenOrientation, "type") => self.text("landscape-primary".into())?,
            (ScreenOrientation, _) => return Err(Error::UnsupportedScreenApi),
            (Screen, _) => return Err(Error::UnsupportedScreenApi),
            // The native context has one bootstrap navigation, no imported
            // browser history and no client-side pushState execution.
            (History, "length") => Value::Number(1.0),
            (History, "state") => Value::Null,
            (History, _) => return Err(Error::UnsupportedHistoryApi),
            (Math, "random") => Value::Builtin(Random),
            (Math, "hypot") => Value::Builtin(Hypot),
            (Math, "sqrt") => Value::Builtin(Sqrt),
            (Object, "create") => Value::Builtin(Create),
            (Object, "keys") => Value::Builtin(Keys),
            (Reflect, "set") => Value::Builtin(Set),
            (Reflect, "construct") => Value::Builtin(Construct),
            (Json, "parse") => Value::Builtin(Parse),
            (Json, "stringify") => Value::Builtin(Stringify),
            (Window, key) => {
                return Err(Error::UnsupportedWindowBinding {
                    property: diagnostic_window_property(key),
                });
            }
            (Navigator, _) => return Err(Error::UnsupportedNavigatorProperty),
            (Math | Object | Reflect | Json, key) => {
                return Err(Error::UnsupportedBuiltinBinding {
                    object: match host {
                        Math => "Math",
                        Object => "Object",
                        Reflect => "Reflect",
                        _ => "JSON",
                    },
                    property: diagnostic_builtin_property(key),
                });
            }
            (Location | Performance, _) => return Err(Error::UnsupportedProfileProperty),
        })
    }

    fn builtin(&mut self, f: Builtin, args: &[Value], depth: usize) -> Result<Value> {
        let a = |i: usize| {
            args.get(i)
                .copied()
                .ok_or(Error::InvalidArguments)
                .and_then(Value::required)
        };
        match f {
            Builtin::IsFinite | Builtin::IsNan => {
                let number = self.number(args.first().copied().unwrap_or(Value::Undefined))?;
                Ok(Value::Bool(if f == Builtin::IsFinite {
                    number.is_finite()
                } else {
                    number.is_nan()
                }))
            }
            Builtin::NumberIsFinite
            | Builtin::NumberIsNan
            | Builtin::NumberIsInteger
            | Builtin::NumberIsSafeInteger => {
                let Value::Number(number) = args
                    .first()
                    .copied()
                    .unwrap_or(Value::Undefined)
                    .required()?
                else {
                    return Ok(Value::Bool(false));
                };
                Ok(Value::Bool(match f {
                    Builtin::NumberIsFinite => number.is_finite(),
                    Builtin::NumberIsNan => number.is_nan(),
                    Builtin::NumberIsInteger => number.is_finite() && number.fract() == 0.0,
                    _ => {
                        number.is_finite()
                            && number.fract() == 0.0
                            && number.abs() <= 9_007_199_254_740_991.0
                    }
                }))
            }
            Builtin::WeakSetAdd | Builtin::WeakSetHas | Builtin::WeakSetDelete => {
                Err(Error::UnsupportedReflectionApi)
            }
            Builtin::Construct => {
                let target = a(0)?;
                if !matches!(target, Value::Host(Host::WeakSetConstructor))
                    || args.get(2).is_some_and(|new_target| {
                        !matches!(new_target, Value::Host(Host::WeakSetConstructor))
                    })
                {
                    return Err(Error::UnsupportedReflectionApi);
                }
                let args = self.items(a(1)?)?;
                let set = self.alloc(Heap::WeakSet(Vec::new()), 0)?;
                if let Some(iterable) = args.first()
                    && !matches!(iterable, Value::Undefined | Value::Null)
                {
                    for key in self.items(*iterable)? {
                        self.weak_set(Builtin::WeakSetAdd, set, key, depth + 1)?;
                    }
                }
                Ok(set)
            }
            Builtin::Bind => Err(Error::UnsupportedNonCallableBinding),
            Builtin::Random => Ok(Value::Number(rand::random::<f64>())),
            Builtin::Now => Ok(Value::Number(
                self.profile.started.elapsed().as_secs_f64() * 1000.0,
            )),
            Builtin::EpochNow => Ok(Value::Number(chrono::Utc::now().timestamp_millis() as f64)),
            Builtin::Sqrt => Ok(Value::Number(
                self.number(args.first().copied().unwrap_or(Value::Undefined))?
                    .sqrt(),
            )),
            Builtin::Hypot => {
                self.budget.work(args.len(), depth)?;
                self.budget.allocate(size_of_val(args))?;
                // Coerce every argument before handling Infinity/NaN. Scale
                // before squaring, and use compensated summation to avoid
                // overflow, underflow and loss from unequal magnitudes.
                let numbers = args
                    .iter()
                    .map(|value| self.number(*value))
                    .collect::<Result<Vec<_>>>()?;
                if numbers.iter().any(|n| n.is_infinite()) {
                    return Ok(Value::Number(f64::INFINITY));
                }
                if numbers.iter().any(|n| n.is_nan()) {
                    return Ok(Value::Number(f64::NAN));
                }
                let maximum = numbers
                    .iter()
                    .fold(0.0_f64, |maximum, n| maximum.max(n.abs()));
                if maximum == 0.0 {
                    return Ok(Value::Number(0.0));
                }
                let mut sum = 0.0;
                let mut compensation = 0.0;
                for number in numbers {
                    let scaled = number / maximum;
                    let term = scaled * scaled - compensation;
                    let next = sum + term;
                    compensation = (next - sum) - term;
                    sum = next;
                }
                Ok(Value::Number(maximum * sum.sqrt()))
            }
            method @ (Builtin::AddEventListener | Builtin::RemoveEventListener) => {
                self.event_listener(method, Value::Host(Host::Window), args, depth + 1)
            }
            Builtin::Create => {
                if !matches!(a(0)?, Value::Null) {
                    return Err(Error::UnsupportedCreatePrototype);
                }
                self.alloc(Heap::NullObject(Vec::new()), 0)
            }
            Builtin::Keys => {
                if let Value::Host(Host::Storage(kind)) = a(0)? {
                    let keys = self.storage_keys(kind)?;
                    let keys = keys
                        .into_iter()
                        .map(|k| self.text(k))
                        .collect::<Result<_>>()?;
                    return self.array(keys);
                }
                let Value::Heap(id) = a(0)? else {
                    return Err(Error::UnsupportedHostEnumeration);
                };
                if matches!(
                    self.heap[id],
                    Heap::Canvas { .. } | Heap::Element { .. } | Heap::Rect(_) | Heap::WeakSet(_)
                ) {
                    return self.array(Vec::new());
                }
                let (Heap::Object(fields) | Heap::NullObject(fields)) = &self.heap[id] else {
                    return Err(Error::UnsupportedHostEnumeration);
                };
                self.budget
                    .allocate(fields.iter().map(|(k, _)| k.len() + 32).sum())?;
                let mut keys: Vec<_> = fields.iter().map(|(k, _)| k.clone()).collect();
                keys.sort_by_key(|k| array_index(k).map_or((1, u32::MAX), |n| (0, n)));
                let keys = keys
                    .into_iter()
                    .map(|k| self.text(k))
                    .collect::<Result<_>>()?;
                self.array(keys)
            }
            Builtin::Set => {
                let value = a(2)?;
                if matches!(a(0)?, Value::Registers) {
                    self.set(a(1)?, value)?;
                } else if matches!(a(0)?, Value::Host(Host::Window)) {
                    let key = self.string(a(1)?, depth + 1)?;
                    if let Some(key) = WINDOW_HANDLERS.iter().find(|name| **name == key) {
                        // EventHandler's legacy non-object-as-null conversion.
                        // Objects and callable identity are retained; this VM
                        // does not dispatch invented input events.
                        let value = if self.is_object(value) {
                            value
                        } else {
                            Value::Null
                        };
                        if !self.window_handlers.contains_key(key) {
                            self.budget.allocate(128)?;
                        }
                        self.window_handlers.insert(key, value);
                        return Ok(Value::Bool(true));
                    }
                    let field = self
                        .observer_globals
                        .as_mut()
                        .and_then(|fields| fields.get_mut(key.as_str()))
                        .ok_or(Error::UnsupportedHostMutation)?;
                    *field = value;
                } else if let Value::Host(Host::Storage(kind)) = a(0)? {
                    let key = self.string(a(1)?, depth + 1)?;
                    let value = self.string(value, depth + 1)?;
                    self.profile
                        .storage(kind)
                        .lock()
                        .map_err(|_| Error::UnsupportedStorage)?
                        .set(key, value)?;
                } else {
                    let key = self.string(a(1)?, depth + 1)?;
                    let Value::Heap(id) = a(0)? else {
                        return Err(Error::UnsupportedHostMutation);
                    };
                    if matches!(self.heap[id], Heap::Style(_)) {
                        // CSSOM's reflected declaration attributes treat null
                        // as empty; all other values use ordinary string coercion.
                        let value = if matches!(value, Value::Null) {
                            String::new()
                        } else {
                            self.string(value, depth + 1)?
                        };
                        if let Heap::Style(style) = &mut self.heap[id] {
                            style.set(&key, &value, &mut self.budget)?;
                        }
                        return Ok(Value::Bool(true));
                    }
                    if matches!(self.heap[id], Heap::Element { .. }) {
                        if key == "ariaHidden" {
                            let value = if matches!(value, Value::Null | Value::Undefined) {
                                None
                            } else {
                                Some(self.string(value, depth + 1)?)
                            };
                            if let Heap::Element { aria_hidden, .. } = &mut self.heap[id] {
                                *aria_hidden = value;
                            }
                            return Ok(Value::Bool(true));
                        }
                        if matches!(
                            key.as_str(),
                            "tagName"
                                | "nodeName"
                                | "nodeType"
                                | "ownerDocument"
                                | "parentNode"
                                | "parentElement"
                        ) {
                            return Ok(Value::Bool(false));
                        }
                        match key.as_str() {
                            "id" | "className" | "textContent" | "innerText" => {}
                            "style" => return Err(Error::UnsupportedCssTextMutation),
                            "innerHTML" | "outerHTML" => {
                                return Err(Error::UnsupportedHtmlMutation);
                            }
                            "outerText" => {
                                return Err(Error::UnsupportedRenderedTextMutation);
                            }
                            _ => {
                                let Heap::Element { kind, .. } = self.heap[id] else {
                                    return Err(Error::MalformedProgram);
                                };
                                return Err(Error::UnsupportedDomMutation {
                                    element: kind.tag(),
                                    property: diagnostic_dom_property(&key),
                                });
                            }
                        }
                        let mut value = if key == "textContent"
                            && matches!(value, Value::Null | Value::Undefined)
                            || key == "innerText" && matches!(value, Value::Null)
                        {
                            String::new()
                        } else {
                            self.string(value, depth + 1)?
                        };
                        self.budget.allocate(value.len())?;
                        let line_breaks = key == "innerText" && value.contains(['\r', '\n']);
                        if key == "innerText" {
                            self.budget.work(value.len(), depth)?;
                            // The setter creates BR nodes for line breaks.
                            // They contribute no descendant text; child-tree
                            // traversal remains explicitly unsupported.
                            value.retain(|c| c != '\r' && c != '\n');
                        }
                        if let Heap::Element {
                            id,
                            class_name,
                            text_content,
                            has_line_breaks,
                            ..
                        } = &mut self.heap[id]
                        {
                            match key.as_str() {
                                "id" => *id = value,
                                "className" => *class_name = value,
                                _ => {
                                    *text_content = value;
                                    *has_line_breaks = line_breaks;
                                }
                            }
                        }
                        return Ok(Value::Bool(true));
                    }
                    if matches!(self.heap[id], Heap::Canvas { .. }) {
                        let number = self.number(value)?;
                        let dimension = if number.is_finite() {
                            number.trunc().rem_euclid(4_294_967_296.0) as u32
                        } else {
                            0
                        };
                        if dimension > 8192 {
                            return Err(Error::AllocationLimit);
                        }
                        if let Heap::Canvas { width, height } = &mut self.heap[id] {
                            match key.as_str() {
                                "width" => *width = dimension,
                                "height" => *height = dimension,
                                _ => return Err(Error::UnsupportedCanvas),
                            }
                        }
                        return Ok(Value::Bool(true));
                    }
                    let (Heap::Object(fields) | Heap::NullObject(fields)) = &mut self.heap[id]
                    else {
                        return Err(match self.heap[id] {
                            Heap::Array(_) => Error::UnsupportedArrayMutation,
                            Heap::Text(_) => Error::UnsupportedTextMutation,
                            Heap::Element { .. } => Error::UnsupportedElementMutation,
                            Heap::Bound { .. } | Heap::Callback { .. } => {
                                Error::UnsupportedCallableMutation
                            }
                            _ => Error::UnsupportedMutation,
                        });
                    };
                    if let Some((_, v)) = fields.iter_mut().find(|(k, _)| k == &key) {
                        *v = value;
                    } else {
                        self.budget.allocate(key.len() + 32)?;
                        fields.push((key, value));
                    }
                }
                Ok(Value::Bool(true))
            }
            Builtin::Parse => {
                let text = self.string(a(0)?, depth + 1)?;
                if text.len() > self.budget.limits.input_bytes {
                    return Err(Error::InputLimit);
                }
                self.budget.allocate(text.len().saturating_mul(32))?;
                let json = serde_json::from_str(&text).map_err(|_| Error::InvalidJson)?;
                self.import(json, depth + 1)
            }
            Builtin::Stringify => self.stringify(a(0)?, depth + 1),
            Builtin::Atob | Builtin::Btoa => {
                let text = self.string(a(0)?, depth + 1)?;
                self.budget.allocate(text.len().saturating_mul(2))?;
                self.text(match f {
                    Builtin::Atob => atob(&text)?,
                    _ => btoa(&text)?,
                })
            }
            Builtin::StorageGet(kind) => {
                let key = self.string(a(0)?, depth + 1)?;
                Ok(self.storage_get(kind, &key)?.unwrap_or(Value::Null))
            }
            Builtin::StorageSet(kind) => {
                let key = self.string(a(0)?, depth + 1)?;
                let value = self.string(a(1)?, depth + 1)?;
                self.profile
                    .storage(kind)
                    .lock()
                    .map_err(|_| Error::UnsupportedStorage)?
                    .set(key, value)?;
                Ok(Value::Undefined)
            }
            Builtin::StorageRemove(kind) => {
                let key = self.string(a(0)?, depth + 1)?;
                self.profile
                    .storage(kind)
                    .lock()
                    .map_err(|_| Error::UnsupportedStorage)?
                    .remove(&key);
                Ok(Value::Undefined)
            }
            Builtin::StorageClear(kind) => {
                self.profile
                    .storage(kind)
                    .lock()
                    .map_err(|_| Error::UnsupportedStorage)?
                    .clear();
                Ok(Value::Undefined)
            }
            Builtin::StorageKey(kind) => {
                let index = self.number(a(0)?)?;
                let index = if index.is_finite() {
                    index.trunc().rem_euclid(4_294_967_296.0) as u32
                } else {
                    0
                };
                let key = {
                    let storage = self
                        .profile
                        .storage(kind)
                        .lock()
                        .map_err(|_| Error::UnsupportedStorage)?;
                    let key = storage.keys().nth(index as usize);
                    if let Some(key) = key {
                        self.budget.allocate(key.len())?;
                    }
                    key.map(str::to_owned)
                };
                key.map_or(Ok(Value::Null), |key| self.text(key))
            }
            Builtin::DocumentAttribute => {
                let attribute = self.string(a(0)?, depth + 1)?;
                if attribute.eq_ignore_ascii_case("data-build") {
                    self.text(self.profile.bootstrap.build.clone())
                } else {
                    Err(Error::UnsupportedDomAttribute)
                }
            }
            Builtin::DocumentQuery => {
                let selector = self.string(a(0)?, depth + 1)?;
                match selector.trim() {
                    "html" | ":root" => Ok(Value::Host(Host::DocumentElement)),
                    _ => Err(Error::UnsupportedDomQuery),
                }
            }
            Builtin::CreateElement => {
                let tag = self.string(a(0)?, depth + 1)?;
                if tag.eq_ignore_ascii_case("canvas") {
                    self.alloc(
                        Heap::Canvas {
                            width: 300,
                            height: 150,
                        },
                        0,
                    )
                } else {
                    let element = match tag.to_ascii_lowercase().as_str() {
                        "iframe" | "frame" => return Err(Error::UnsupportedFrameEnvironment),
                        "script" => return Err(Error::UnsupportedScriptExecution),
                        "div" => Element::Div,
                        "span" => Element::Span,
                        "body" => Element::Body,
                        "html" => Element::Html,
                        "a" => Element::Anchor,
                        "input" => Element::Input,
                        _ => return Err(Error::UnsupportedDomCreation),
                    };
                    let style = self.alloc(Heap::Style(InlineStyle::default()), 0)?;
                    self.alloc(
                        Heap::Element {
                            kind: element,
                            style,
                            id: String::new(),
                            class_name: String::new(),
                            text_content: String::new(),
                            aria_hidden: None,
                            connected: false,
                            has_line_breaks: false,
                        },
                        0,
                    )
                }
            }
            Builtin::CanvasContext => {
                // This native context has no drawing backends. The HTML canvas
                // contract represents an unavailable context with null. A
                // program that requires GPU/pixel values still fails on use.
                let _context = self.string(a(0)?, depth + 1)?;
                Ok(Value::Null)
            }
            Builtin::StringSplit => {
                self.exception = Some(GuestException {
                    description: "TypeError: String.prototype.split called on null or undefined"
                        .into(),
                    failure: Error::RequiredUndefinedProperty,
                });
                Err(Error::GuestException)
            }
            Builtin::ArrayPop => {
                self.exception = Some(GuestException {
                    description: "TypeError: Cannot convert undefined or null to object".into(),
                    failure: Error::RequiredUndefinedProperty,
                });
                Err(Error::GuestException)
            }
            Builtin::BodyAppendChild
            | Builtin::BodyRemoveChild
            | Builtin::ElementRemove
            | Builtin::ElementRect
            | Builtin::RectToJson => {
                self.exception = Some(GuestException {
                    description: "TypeError: Illegal invocation".into(),
                    failure: Error::UnsupportedDomTreeMutation,
                });
                Err(Error::GuestException)
            }
            Builtin::CanvasSerialize => Err(Error::UnsupportedCanvasSerialization),
            Builtin::MeasureLayout => Err(Error::UnsupportedDomLayout),
            // A first-class Promise outside call-and-set is unimplemented.
            Builtin::EstimateStorage => Err(Error::UnsupportedStorageEstimate),
        }
    }

    fn mutate_body(
        &mut self,
        method: Builtin,
        receiver: Value,
        args: &[Value],
        depth: usize,
    ) -> Result<Value> {
        self.budget.step(depth)?;
        let child = if method == Builtin::ElementRemove {
            receiver
        } else {
            if !matches!(receiver, Value::Host(Host::Body)) {
                return Err(Error::UnsupportedDomTreeMutation);
            }
            *args.first().ok_or(Error::InvalidArguments)?
        };
        let Value::Heap(id) = child else {
            return Err(Error::UnsupportedDomTreeMutation);
        };
        let Heap::Element { connected, .. } = &mut self.heap[id] else {
            return Err(Error::UnsupportedDomTreeMutation);
        };
        if method == Builtin::BodyRemoveChild && !*connected {
            return Err(Error::UnsupportedDomTreeMutation);
        }
        if method == Builtin::BodyAppendChild {
            self.budget.allocate(size_of::<Value>())?;
        }
        self.body_children
            .retain(|v| !matches!(v, Value::Heap(index) if *index == id));
        *connected = method == Builtin::BodyAppendChild;
        if *connected {
            self.body_children.push(child);
        }
        Ok(if method == Builtin::ElementRemove {
            Value::Undefined
        } else {
            child
        })
    }

    fn split_string(&mut self, receiver: Value, args: &[Value], depth: usize) -> Result<Value> {
        self.budget.step(depth)?;
        let text = self.string(receiver, depth + 1)?;
        let separator = args
            .first()
            .copied()
            .unwrap_or(Value::Undefined)
            .required()?;
        let limit = args
            .get(1)
            .copied()
            .unwrap_or(Value::Undefined)
            .required()?;
        let limit = if matches!(limit, Value::Undefined) {
            u32::MAX as usize
        } else {
            let value = self.number(limit)?;
            if value.is_finite() {
                value.trunc().rem_euclid(4_294_967_296.0) as usize
            } else {
                0
            }
        };
        if limit == 0 {
            return self.array(Vec::new());
        }
        if matches!(separator, Value::Undefined) {
            let text = self.text(text)?;
            return self.array(vec![text]);
        }
        if matches!(separator, Value::Heap(id) if !matches!(self.heap[id], Heap::Text(_)))
            || matches!(
                separator,
                Value::Host(_) | Value::Builtin(_) | Value::Opcode(_) | Value::Registers
            )
        {
            return Err(Error::UnsupportedStringApi);
        }
        let separator = self.string(separator, depth + 1)?;
        let mut parts = Vec::new();
        if separator.is_empty() {
            // ECMAScript splits UTF-16 code units. Unpaired surrogates remain
            // explicitly unsupported by this UTF-8 arena, never replaced.
            for unit in text.encode_utf16().take(limit) {
                self.budget.step(depth)?;
                self.budget.allocate(size_of::<Value>() + 4)?;
                let part = String::from_utf16(&[unit]).map_err(|_| Error::UnsupportedString)?;
                parts.push(self.text(part)?);
            }
        } else {
            self.budget
                .work(text.len().saturating_add(separator.len()), depth)?;
            for part in text.split(&separator).take(limit) {
                self.budget.step(depth)?;
                self.budget.allocate(size_of::<Value>() + part.len())?;
                parts.push(self.text(part.to_owned())?);
            }
        }
        self.array(parts)
    }

    fn boolean(&self, value: Value) -> Result<bool> {
        Ok(match value.required()? {
            Value::Undefined | Value::Null => false,
            Value::Bool(value) => value,
            Value::Number(value) => value != 0.0 && !value.is_nan(),
            Value::Heap(id) => !matches!(&self.heap[id], Heap::Text(text) if text.is_empty()),
            _ => true,
        })
    }

    fn event_listener(
        &mut self,
        method: Builtin,
        target: Value,
        args: &[Value],
        depth: usize,
    ) -> Result<Value> {
        self.budget.step(depth)?;
        if !matches!(
            target,
            Value::Host(Host::Window | Host::Document | Host::Body | Host::DocumentElement)
        ) && !matches!(target, Value::Heap(id) if matches!(self.heap[id], Heap::Element { .. }))
        {
            return Err(Error::UnsupportedEventTarget { reason: "receiver" });
        }
        let event_type = self.string(*args.first().ok_or(Error::InvalidArguments)?, depth + 1)?;
        if event_type.len() > 128 {
            return Err(Error::InputLimit);
        }
        let callback = args
            .get(1)
            .copied()
            .ok_or(Error::InvalidArguments)?
            .required()?;
        if !matches!(callback, Value::Null | Value::Undefined) && !self.is_callable(callback) {
            return Err(Error::UnsupportedEventTarget {
                reason: "callback_object",
            });
        }
        let mut options = 0;
        let dictionary = args
            .get(2)
            .copied()
            .unwrap_or(Value::Undefined)
            .required()?;
        match dictionary {
            Value::Null | Value::Undefined => {}
            Value::Bool(value) => options = u8::from(value),
            Value::Heap(id) if matches!(self.heap[id], Heap::Object(_) | Heap::NullObject(_)) => {
                let keys: &[(&str, u8)] = if method == Builtin::AddEventListener {
                    &[("capture", 1), ("once", 2), ("passive", 4)]
                } else {
                    &[("capture", 1)]
                };
                for (key, flag) in keys {
                    let key = self.text((*key).to_owned())?;
                    let value = self.property(dictionary, key, depth + 1)?;
                    if self.boolean(value)? {
                        options |= flag;
                    }
                }
                if method == Builtin::AddEventListener {
                    let key = self.text("signal".into())?;
                    if !matches!(self.property(dictionary, key, depth + 1)?, Value::Undefined) {
                        return Err(Error::UnsupportedEventTarget {
                            reason: "abort_signal",
                        });
                    }
                }
            }
            _ => return Err(Error::UnsupportedEventTarget { reason: "options" }),
        }
        if matches!(callback, Value::Null | Value::Undefined) {
            return Ok(Value::Undefined);
        }
        let mut existing = None;
        for (index, listener) in self.listeners.iter().enumerate() {
            self.budget.step(depth)?;
            if listener.event_type == event_type
                && listener.options & 1 == options & 1
                && self.equal(listener.target, target)?
                && self.equal(listener.callback, callback)?
            {
                existing = Some(index);
                break;
            }
        }
        match (method, existing) {
            (Builtin::RemoveEventListener, Some(index)) => {
                self.listeners.remove(index);
            }
            (Builtin::AddEventListener, None) => {
                if self.listeners.len() >= 256 {
                    return Err(Error::AllocationLimit);
                }
                self.budget
                    .allocate(event_type.len() + 2 * size_of::<EventListener>())?;
                self.listeners.push(EventListener {
                    target,
                    event_type,
                    callback,
                    options,
                });
            }
            _ => {}
        }
        Ok(Value::Undefined)
    }

    fn element_rect(&mut self, receiver: Value) -> Result<Value> {
        let Value::Heap(id) = receiver else {
            return Err(Error::UnsupportedDomLayout);
        };
        let Heap::Element {
            kind,
            style,
            text_content,
            connected,
            has_line_breaks,
            ..
        } = &self.heap[id]
        else {
            return Err(Error::UnsupportedDomLayout);
        };
        if !connected {
            return self.alloc(Heap::Rect([0.0; 4]), 0);
        }
        if !matches!(kind, Element::Div | Element::Span | Element::Anchor) {
            return Err(Error::UnsupportedNativeLayout {
                reason: "element_kind",
            });
        }
        if *has_line_breaks {
            return Err(Error::UnsupportedNativeLayout {
                reason: "rendered_line_breaks",
            });
        }
        // The static position of auto insets is defined only while all body
        // children are out of flow. Other DOM/layout contexts stay unavailable.
        for child in &self.body_children {
            let Value::Heap(child) = child else {
                return Err(Error::UnsupportedDomLayout);
            };
            let Heap::Element {
                style: Value::Heap(style),
                ..
            } = &self.heap[*child]
            else {
                return Err(Error::UnsupportedDomLayout);
            };
            let Heap::Style(style) = &self.heap[*style] else {
                return Err(Error::UnsupportedDomLayout);
            };
            self.budget.step(0)?;
            if style.get("display")? != "none"
                && !matches!(style.get("position")?, "absolute" | "fixed")
            {
                return Err(Error::UnsupportedNativeLayout {
                    reason: "body_flow",
                });
            }
        }
        let Value::Heap(style) = style else {
            return Err(Error::UnsupportedDomLayout);
        };
        let Heap::Style(style) = &self.heap[*style] else {
            return Err(Error::UnsupportedDomLayout);
        };
        let rect = self.layout.rect(style, text_content, &mut self.budget)?;
        self.alloc(Heap::Rect(rect), 0)
    }

    fn storage_get(&mut self, kind: StorageKind, key: &str) -> Result<Option<Value>> {
        let value = {
            let storage = self
                .profile
                .storage(kind)
                .lock()
                .map_err(|_| Error::UnsupportedStorage)?;
            let value = storage.get(key);
            if let Some(value) = value {
                self.budget.allocate(value.len())?;
            }
            value.map(str::to_owned)
        };
        value.map(|value| self.text(value)).transpose()
    }

    fn storage_keys(&mut self, kind: StorageKind) -> Result<Vec<String>> {
        let storage = self
            .profile
            .storage(kind)
            .lock()
            .map_err(|_| Error::UnsupportedStorage)?;
        self.budget
            .allocate(storage.keys().map(|k| k.len() + 32).sum())?;
        Ok(storage.keys().map(str::to_owned).collect())
    }
}

fn rect_fields([x, y, width, height]: [f64; 4]) -> [(&'static str, f64); 8] {
    [
        ("x", x),
        ("y", y),
        ("width", width),
        ("height", height),
        ("top", y.min(y + height)),
        ("right", x.max(x + width)),
        ("bottom", y.max(y + height)),
        ("left", x.min(x + width)),
    ]
}

fn diagnostic_window_property(key: &str) -> &'static str {
    const NAMES: &[&str] = &[
        "__proto__",
        "__defineGetter__",
        "__defineSetter__",
        "__lookupGetter__",
        "__lookupSetter__",
        "__sentinel_script_loads",
        "__oai_SSR_HTML",
        "__STATSIG__",
        "__REACT_QUERY_CACHE__",
        "__mobxGlobals",
        "__mobxInstanceCount",
        "__oai_unstructured_event_v1__",
        "__oaiAnchorPositioningPolyfill",
        "__oaiBankCallbackParameters",
        "__oaiEpicNativeLaunch",
        "__oaiMobileWebHandoffInspection",
        "addEventListener",
        "removeEventListener",
        "dispatchEvent",
        "Event",
        "EventTarget",
        "MutationObserver",
        "ResizeObserver",
        "IntersectionObserver",
        "RegExp",
        "Error",
        "TypeError",
        "Intl",
        "URL",
        "URLSearchParams",
        "Image",
        "fetch",
        "XMLHttpRequest",
        "WebSocket",
        "queueMicrotask",
        "structuredClone",
        "getSelection",
        "matchMedia",
        "visualViewport",
        "origin",
        "name",
        "opener",
        "closed",
        "length",
        "onload",
        "onerror",
        "onfocus",
        "onblur",
        "requestIdleCallback",
        "cancelIdleCallback",
        "cancelAnimationFrame",
        "webkitRequestAnimationFrame",
        "chrome",
        "Notification",
        "Boolean",
        "BigInt",
        "parseInt",
        "parseFloat",
        "Proxy",
        "Symbol",
        "Map",
        "Set",
        "WeakMap",
        "WeakSet",
        "PerformanceObserver",
        "PerformanceEntry",
        "MouseEvent",
        "PointerEvent",
        "KeyboardEvent",
        "TouchEvent",
        "CustomEvent",
        "UIEvent",
        "MessageEvent",
        "FocusEvent",
        "ErrorEvent",
        "AbortController",
        "AbortSignal",
        "MessageChannel",
        "MessagePort",
        "Text",
        "Node",
        "Element",
        "HTMLElement",
        "Document",
        "Window",
        "DOMRect",
        "DOMException",
        "DOMParser",
        "MutationRecord",
        "URLPattern",
        "Blob",
        "File",
        "FileReader",
        "FormData",
        "Headers",
        "Request",
        "Response",
        "ImageData",
        "FontFace",
        "console",
        "encodeURI",
        "encodeURIComponent",
        "decodeURI",
        "decodeURIComponent",
        "escape",
        "unescape",
        "reportError",
        "navigation",
        "scheduler",
        "postMessage",
        "indexedDB",
        "caches",
        "frameElement",
        "external",
        "customElements",
        "trustedTypes",
        "onmousemove",
        "onmousedown",
        "onmouseup",
        "onkeydown",
        "onkeyup",
        "onpointermove",
        "onpointerdown",
        "onpointerup",
        "ontouchstart",
        "ontouchmove",
        "onpagehide",
        "onpageshow",
        "onbeforeunload",
        "onunload",
        "onresize",
        "onscroll",
        "onmessage",
        "ononline",
        "onoffline",
    ];
    NAMES
        .iter()
        .copied()
        .find(|name| *name == key)
        .unwrap_or(if key.starts_with("__") {
            "application_private_global"
        } else {
            "unrecognized"
        })
}

fn diagnostic_builtin_property(key: &str) -> &'static str {
    const NAMES: &[&str] = &[
        "abs",
        "isFinite",
        "isNaN",
        "isInteger",
        "isSafeInteger",
        "parseInt",
        "parseFloat",
        "MAX_VALUE",
        "MIN_VALUE",
        "MAX_SAFE_INTEGER",
        "MIN_SAFE_INTEGER",
        "EPSILON",
        "NaN",
        "POSITIVE_INFINITY",
        "NEGATIVE_INFINITY",
        "floor",
        "ceil",
        "trunc",
        "round",
        "min",
        "max",
        "sqrt",
        "pow",
        "hypot",
        "imul",
        "clz32",
        "fround",
        "sin",
        "cos",
        "tan",
        "atan",
        "atan2",
        "exp",
        "log",
        "log2",
        "log10",
        "sign",
        "PI",
        "E",
        "random",
        "assign",
        "entries",
        "values",
        "keys",
        "create",
        "freeze",
        "seal",
        "hasOwn",
        "getPrototypeOf",
        "setPrototypeOf",
        "getOwnPropertyNames",
        "getOwnPropertyDescriptor",
        "getOwnPropertyDescriptors",
        "defineProperty",
        "defineProperties",
        "is",
        "isExtensible",
        "isFrozen",
        "isSealed",
        "apply",
        "construct",
        "get",
        "set",
        "ownKeys",
        "deleteProperty",
        "has",
        "parse",
        "stringify",
    ];
    NAMES
        .iter()
        .copied()
        .find(|name| *name == key)
        .unwrap_or("unrecognized")
}

// Register maps distinguish numbers from strings. The magnitude of a numeric
// register does not allocate a dense array; the shared map budget limits storage.
fn numeric_key(value: f64) -> String {
    let bits = if value == 0.0 { 0 } else { value.to_bits() };
    format!("n:{bits:016x}")
}

fn array_index(key: &str) -> Option<u32> {
    key.parse::<u32>()
        .ok()
        .filter(|n| *n != u32::MAX && n.to_string() == key)
}

// Only public Web IDL property names can enter diagnostics. Unknown challenge
// operands, including expando names, are never reflected into reports.
fn diagnostic_dom_property(key: &str) -> &'static str {
    const PROPERTIES: &[&str] = &[
        "title",
        "lang",
        "translate",
        "dir",
        "hidden",
        "inert",
        "accessKey",
        "draggable",
        "spellcheck",
        "autocapitalize",
        "contentEditable",
        "enterKeyHint",
        "inputMode",
        "popover",
        "nonce",
        "autofocus",
        "tabIndex",
        "download",
        "ping",
        "rel",
        "referrerPolicy",
        "text",
        "name",
        "target",
        "href",
        "protocol",
        "host",
        "hostname",
        "port",
        "pathname",
        "search",
        "hash",
        "hreflang",
        "type",
        "accept",
        "alt",
        "autocomplete",
        "checked",
        "disabled",
        "files",
        "height",
        "max",
        "min",
        "maxLength",
        "minLength",
        "multiple",
        "pattern",
        "placeholder",
        "readOnly",
        "required",
        "size",
        "src",
        "step",
        "defaultValue",
        "value",
        "valueAsDate",
        "valueAsNumber",
        "width",
        "selectionStart",
        "selectionEnd",
        "selectionDirection",
        "slot",
        "part",
        "scrollTop",
        "scrollLeft",
        "role",
        "ariaHidden",
        "ariaLabel",
        "nodeValue",
        "onload",
        "onerror",
        "onclick",
        "onchange",
        "oninput",
        "append",
        "appendChild",
        "removeChild",
        "replaceChildren",
        "children",
        "childNodes",
        "childElementCount",
        "firstChild",
        "lastChild",
        "parentNode",
        "style",
        "textContent",
        "innerText",
        "getBoundingClientRect",
        "offsetWidth",
        "offsetHeight",
        "clientWidth",
        "clientHeight",
    ];
    PROPERTIES
        .iter()
        .copied()
        .find(|name| *name == key)
        .unwrap_or("unrecognized")
}

fn diagnostic_js_property(key: &str) -> &'static str {
    OBJECT_PROPERTIES
        .iter()
        .chain(ARRAY_METHODS)
        .chain(STRING_METHODS)
        .chain(FUNCTION_PROPERTIES)
        .copied()
        .find(|name| *name == key)
        .unwrap_or("unrecognized")
}

const FUNCTION_PROPERTIES: &[&str] = &[
    "name",
    "length",
    "call",
    "apply",
    "bind",
    "prototype",
    "arguments",
    "caller",
];

const OBJECT_PROPERTIES: &[&str] = &[
    "constructor",
    "toString",
    "valueOf",
    "toLocaleString",
    "hasOwnProperty",
    "isPrototypeOf",
    "propertyIsEnumerable",
    "__proto__",
    "__defineGetter__",
    "__defineSetter__",
    "__lookupGetter__",
    "__lookupSetter__",
];

fn inherited_property(key: &str) -> bool {
    OBJECT_PROPERTIES.contains(&key)
}

const ARRAY_METHODS: &[&str] = &[
    "at",
    "concat",
    "copyWithin",
    "entries",
    "every",
    "fill",
    "filter",
    "find",
    "findIndex",
    "findLast",
    "findLastIndex",
    "flat",
    "flatMap",
    "forEach",
    "includes",
    "indexOf",
    "join",
    "keys",
    "lastIndexOf",
    "map",
    "pop",
    "push",
    "reduce",
    "reduceRight",
    "reverse",
    "shift",
    "slice",
    "some",
    "sort",
    "splice",
    "toReversed",
    "toSorted",
    "toSpliced",
    "unshift",
    "values",
    "with",
];

fn array_method(key: &str) -> bool {
    ARRAY_METHODS.contains(&key)
}

const STRING_METHODS: &[&str] = &[
    "at",
    "charAt",
    "charCodeAt",
    "codePointAt",
    "concat",
    "endsWith",
    "includes",
    "indexOf",
    "isWellFormed",
    "lastIndexOf",
    "localeCompare",
    "match",
    "matchAll",
    "normalize",
    "padEnd",
    "padStart",
    "repeat",
    "replace",
    "replaceAll",
    "search",
    "slice",
    "split",
    "startsWith",
    "substring",
    "toLocaleLowerCase",
    "toLocaleUpperCase",
    "toLowerCase",
    "toUpperCase",
    "toWellFormed",
    "trim",
    "trimEnd",
    "trimStart",
];

fn string_method(key: &str) -> bool {
    STRING_METHODS.contains(&key)
}

fn numeric_string(value: &str) -> Result<f64> {
    static DECIMAL: std::sync::LazyLock<std::result::Result<regex::Regex, regex::Error>> =
        std::sync::LazyLock::new(|| {
            regex::Regex::new(
                r"\A[+-]?(?:(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+)(?:[eE][+-]?[0-9]+)?|Infinity)\z",
            )
        });
    let value = value.trim_matches(|c| {
        matches!(
            c,
            '\t' | '\n' | '\x0b' | '\x0c' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
                ..='\u{200a}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202f}'
                    | '\u{205f}'
                    | '\u{3000}'
                    | '\u{feff}'
        )
    });
    if value.is_empty() {
        return Ok(0.0);
    }
    for (prefixes, radix) in [(["0x", "0X"], 16), (["0o", "0O"], 8), (["0b", "0B"], 2)] {
        if let Some(digits) = prefixes
            .iter()
            .find_map(|prefix| value.strip_prefix(prefix))
        {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return Ok(f64::NAN);
            }
            let digits = digits.trim_start_matches('0');
            if digits.is_empty() {
                return Ok(0.0);
            }
            return u64::from_str_radix(digits, radix)
                .map(|n| n as f64)
                .map_err(|_| Error::UnsupportedNumericCoercion);
        }
    }
    if !DECIMAL
        .as_ref()
        .map_err(|_| Error::UnsupportedNumericCoercion)?
        .is_match(value)
    {
        return Ok(f64::NAN);
    }
    value.parse().map_err(|_| Error::UnsupportedNumericCoercion)
}

fn number_string(n: f64) -> Result<String> {
    if n.is_nan() {
        return Ok("NaN".into());
    }
    if n.is_infinite() {
        return Ok(if n.is_sign_positive() {
            "Infinity"
        } else {
            "-Infinity"
        }
        .into());
    }
    if n == 0.0 {
        return Ok("0".into());
    }
    // This runtime only exposes the proven decimal subset; don't silently use
    // Rust formatting where JavaScript exponent/rounding semantics may differ.
    if n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0 {
        return Ok(format!("{n:.0}"));
    }
    if n.fract() == 0.0 {
        return Err(Error::UnsupportedString);
    }
    if (1e-6..1e21).contains(&n.abs()) {
        return Ok(n.to_string());
    }
    Err(Error::UnsupportedString)
}

fn xor(data: &str, key: &str) -> Result<String> {
    let key: Vec<_> = key.encode_utf16().collect();
    if key.is_empty() {
        return Err(Error::MalformedProgram);
    }
    let data: Vec<_> = data
        .encode_utf16()
        .enumerate()
        .map(|(i, c)| c ^ key[i % key.len()])
        .collect();
    String::from_utf16(&data).map_err(|_| Error::UnsupportedString)
}

fn btoa(text: &str) -> Result<String> {
    let bytes = text
        .encode_utf16()
        .map(|c| u8::try_from(c).map_err(|_| Error::UnsupportedString))
        .collect::<Result<Vec<_>>>()?;
    Ok(BASE64_STANDARD.encode(bytes))
}

fn atob(text: &str) -> Result<String> {
    BASE64_STANDARD
        .decode(text)
        .map(|bytes| bytes.into_iter().map(char::from).collect())
        .map_err(|_| Error::MalformedProgram)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sentinel::Bootstrap;
    use serde_json::json;

    fn profile() -> Profile {
        Profile::new(Bootstrap {
            origin: "https://chatgpt.com".into(),
            build: "fixture".into(),
            scripts: vec!["https://chatgpt.com/app.js".into()],
            router: Err(Error::UnsupportedRouterContext),
        })
    }

    fn encode(program: Json, key: &str) -> String {
        let plain: String = program
            .to_string()
            .encode_utf16()
            .map(|unit| {
                if unit <= 0x7f {
                    char::from(unit as u8).to_string()
                } else {
                    format!("\\u{unit:04x}")
                }
            })
            .collect();
        btoa(&xor(&plain, key).unwrap()).unwrap()
    }

    #[test]
    fn arena_budget_covers_heap_slots_and_diagnostics_do_not_inflate_values() {
        assert!(size_of::<Value>() <= 16);
        let profile = profile();
        let mut vm = Vm::new(&profile, "key", CancellationToken::new(), Limits::default()).unwrap();
        vm.budget.limits.allocation_bytes = vm.budget.allocated + 2 * size_of::<Heap>() - 1;
        assert!(matches!(
            vm.text(String::new()),
            Err(Error::AllocationLimit)
        ));
    }

    #[test]
    fn string_split_uses_bound_receiver_literal_separators_and_limits() {
        for (text, separator, limit, expected) in [
            ("a,b,,c", json!(","), None, json!(["a", "b", "", "c"])),
            ("a,b,c", json!(","), Some(2), json!(["a", "b"])),
            ("abc", json!(""), Some(2), json!(["a", "b"])),
            ("a.b", json!("."), None, json!(["a", "b"])),
            ("anullb", Json::Null, None, json!(["a", "b"])),
            ("abc", json!(","), Some(0), json!([])),
            ("😀é😀", json!("😀"), None, json!(["", "é", ""])),
        ] {
            let mut program = vec![
                json!([2, 40, text]),
                json!([2, 41, "split"]),
                json!([24, 42, 40, 41]),
                json!([2, 43, separator]),
            ];
            if let Some(limit) = limit {
                program.push(json!([2, 44, limit]));
            }
            program.extend([
                json!([17, 45, 42, 43, 44]),
                json!([15, 46, 45]),
                json!([7, 3, 46]),
            ]);
            assert_eq!(
                solve(
                    &encode(json!(program), "key"),
                    "key",
                    &profile(),
                    CancellationToken::new()
                ),
                btoa(&expected.to_string())
            );
        }
        for (text, operation, expected) in [
            ("abc", 24, Ok(BASE64_STANDARD.encode(r#"["abc"]"#))),
            (
                "abc",
                6,
                Ok(BASE64_STANDARD
                    .encode(r#""TypeError: String.prototype.split called on null or undefined""#)),
            ),
        ] {
            let program = json!([
                [2, 40, text],
                [2, 41, "split"],
                [operation, 42, 40, 41],
                [17, 43, 42],
                [15, 44, 43],
                [7, 3, 44]
            ]);
            assert_eq!(
                solve(
                    &encode(program, "key"),
                    "key",
                    &profile(),
                    CancellationToken::new()
                ),
                expected
            );
        }
        let program = json!([
            [2, 40, "😀"],
            [2, 41, "split"],
            [24, 42, 40, 41],
            [2, 43, ""],
            [17, 44, 42, 43],
            [3, "unreachable"]
        ]);
        assert_eq!(
            solve(
                &encode(program, "key"),
                "key",
                &profile(),
                CancellationToken::new()
            ),
            Err(Error::UnsupportedString)
        );
    }

    #[test]
    fn structural_property_diagnostics_never_copy_unknown_operands() {
        assert_eq!(diagnostic_dom_property("ariaHidden"), "ariaHidden");
        assert_eq!(diagnostic_js_property("split"), "split");
        for value in
            ["private-token", "Bearer secret", "a"].map(|key| Error::UnsupportedDomMutation {
                element: "DIV",
                property: diagnostic_dom_property(key),
            })
        {
            assert_eq!(
                serde_json::to_string(&value).unwrap(),
                r#"{"unsupported_dom_mutation":{"element":"DIV","property":"unrecognized"}}"#
            );
        }
        assert_eq!(diagnostic_js_property("private-token"), "unrecognized");
    }

    #[test]
    fn array_pop_mutates_the_bound_array_and_returns_undefined_when_empty() {
        let program = json!([
            [2, 40, ["first", "last"]],
            [8, 41, 40],
            [2, 42, "pop"],
            [24, 43, 40, 42],
            [17, 44, 43],
            [15, 45, 41],
            [2, 46, []],
            [5, 46, 44],
            [5, 46, 45],
            [17, 47, 43],
            [17, 48, 43],
            [5, 46, 48],
            [15, 49, 46],
            [7, 3, 49]
        ]);
        assert_eq!(
            solve(
                &encode(program, "key"),
                "key",
                &profile(),
                CancellationToken::new()
            ),
            Ok(BASE64_STANDARD.encode(json!(["last", "[\"first\"]", null]).to_string()))
        );
    }

    #[test]
    fn hypot_matches_browser_extremes_and_numeric_string_coercion() {
        let profile = profile();
        let mut vm = Vm::new(&profile, "key", CancellationToken::new(), Limits::default()).unwrap();
        for (input, expected) in [
            (vec![], 0.0),
            (vec![3.0, 4.0], 5.0),
            (vec![3.0, 4.0, 12.0], 13.0),
            (vec![1.0, 1.0], std::f64::consts::SQRT_2),
            (vec![1e308, 1e308], 1.414_213_562_373_095_1e308),
            (
                vec![f64::from_bits(1), f64::from_bits(1)],
                f64::from_bits(1),
            ),
            (vec![f64::INFINITY, f64::NAN], f64::INFINITY),
            (vec![-0.0, -0.0], 0.0),
        ] {
            let args: Vec<_> = input.into_iter().map(Value::Number).collect();
            let Value::Number(result) = vm.builtin(Builtin::Hypot, &args, 0).unwrap() else {
                panic!("expected a number")
            };
            assert_eq!(result.to_bits(), expected.to_bits());
        }
        for (input, expected) in [
            ("0x10", 16.0),
            ("0b11", 3.0),
            ("0o77", 63.0),
            ("\u{feff}42\u{feff}", 42.0),
            (".5", 0.5),
            ("1.", 1.0),
            ("-.5", -0.5),
            ("1e309", f64::INFINITY),
            ("+Infinity", f64::INFINITY),
            ("-0", -0.0),
        ] {
            assert_eq!(numeric_string(input).unwrap().to_bits(), expected.to_bits());
        }
        for input in ["inf", "-0x10", "0x", "\u{85}42\u{85}", "1_000"] {
            assert!(numeric_string(input).unwrap().is_nan());
        }
        assert_eq!(
            numeric_string("0x10000000000000000"),
            Err(Error::UnsupportedNumericCoercion)
        );
    }

    #[test]
    fn native_listeners_preserve_identity_options_and_do_not_fabricate_events() {
        let profile = profile();
        let cancel = CancellationToken::new();
        let mut vm = Vm::new(&profile, "key", cancel.clone(), Limits::default()).unwrap();
        let event = vm.text("synthetic-fixture".into()).unwrap();
        let callback = Value::Opcode(3);
        let once = vm.import(json!({"once":true,"passive":true}), 0).unwrap();
        let window = Value::Host(Host::Window);
        let document = Value::Host(Host::Document);
        vm.event_listener(
            Builtin::AddEventListener,
            window,
            &[event, callback, once],
            0,
        )
        .unwrap();
        vm.event_listener(
            Builtin::AddEventListener,
            window,
            &[event, callback, Value::Bool(false)],
            0,
        )
        .unwrap();
        assert_eq!(vm.listeners.len(), 1);
        assert_eq!(vm.listeners[0].options, 6);
        vm.event_listener(Builtin::AddEventListener, document, &[event, callback], 0)
            .unwrap();
        vm.event_listener(
            Builtin::AddEventListener,
            window,
            &[event, callback, Value::Bool(true)],
            0,
        )
        .unwrap();
        assert_eq!(vm.listeners.len(), 3);
        vm.event_listener(
            Builtin::RemoveEventListener,
            window,
            &[event, callback, Value::Bool(false)],
            0,
        )
        .unwrap();
        assert_eq!(vm.listeners.len(), 2);
        vm.event_listener(Builtin::AddEventListener, window, &[event, Value::Null], 0)
            .unwrap();
        assert_eq!(vm.listeners.len(), 2);
        assert!(vm.result.is_none());
        assert!(matches!(
            vm.host_property(Host::Window, "dispatchEvent"),
            Err(Error::UnsupportedEventTarget {
                reason: "dispatch_source"
            })
        ));
        cancel.cancel();
        assert!(matches!(
            vm.event_listener(Builtin::RemoveEventListener, window, &[event, callback], 0),
            Err(Error::Cancelled)
        ));
    }

    #[test]
    fn date_now_uses_native_epoch_milliseconds_and_date_uses_declared_utc() {
        let program = json!([
            [2, 40, "Date"],
            [6, 41, 10, 40],
            [2, 42, "now"],
            [6, 43, 41, 42],
            [17, 44, 43],
            [7, 3, 44]
        ]);
        let before = chrono::Utc::now().timestamp_millis();
        let encoded = solve(
            &encode(program, "key"),
            "key",
            &profile(),
            CancellationToken::new(),
        )
        .unwrap();
        let after = chrono::Utc::now().timestamp_millis();
        let timestamp = String::from_utf8(BASE64_STANDARD.decode(encoded).unwrap())
            .unwrap()
            .parse::<i64>()
            .unwrap();
        assert!((before..=after).contains(&timestamp));
        let program = json!([[2, 40, "Date"], [6, 41, 10, 40], [17, 42, 41], [7, 3, 42]]);
        let encoded = solve(
            &encode(program, "key"),
            "key",
            &profile(),
            CancellationToken::new(),
        )
        .unwrap();
        let value = String::from_utf8(BASE64_STANDARD.decode(encoded).unwrap()).unwrap();
        assert!(value.ends_with(" GMT+0000 (Coordinated Universal Time)"));
    }

    #[test]
    fn native_rect_uses_bound_element_state_and_serializes_browser_fields() {
        let mut program = json!([
            [2, 40, "document"],
            [6, 41, 10, 40],
            [2, 42, "createElement"],
            [24, 43, 41, 42],
            [2, 44, "div"],
            [17, 45, 43, 44],
            [2, 46, "style"],
            [6, 47, 45, 46],
            [2, 48, "Reflect"],
            [6, 49, 10, 48],
            [2, 50, "set"],
            [6, 51, 49, 50]
        ])
        .as_array()
        .unwrap()
        .clone();
        for (key, value) in [
            ("position", "absolute"),
            ("left", "10.2px"),
            ("top", "20.3px"),
            ("fontFamily", "DejaVu Sans"),
            ("fontSize", "16px"),
        ] {
            program.extend([
                json!([2, 52, key]),
                json!([2, 53, value]),
                json!([17, 54, 51, 47, 52, 53]),
            ]);
        }
        program.extend(
            json!([
                [2, 52, "textContent"],
                [2, 53, "Native layout"],
                [17, 54, 51, 45, 52, 53],
                [2, 55, "body"],
                [6, 56, 41, 55],
                [2, 57, "appendChild"],
                [24, 58, 56, 57],
                [17, 59, 58, 45],
                [2, 60, "getBoundingClientRect"],
                [24, 61, 45, 60],
                [17, 62, 61],
                [15, 63, 62],
                [7, 3, 63]
            ])
            .as_array()
            .unwrap()
            .iter()
            .cloned(),
        );
        assert_eq!(solve(&encode(json!(program),"key"),"key",&profile(),CancellationToken::new()),Ok(BASE64_STANDARD.encode(r#"{"x":10.1875,"y":20.296875,"width":106.8125,"height":19,"top":20.296875,"right":117,"bottom":39.296875,"left":10.1875}"#)));
    }

    #[test]
    fn owned_nodes_attach_and_detach_and_require_a_supported_layout_context() {
        let prefix = json!([
            [2, 40, "document"],
            [6, 41, 10, 40],
            [2, 42, "createElement"],
            [24, 43, 41, 42],
            [2, 44, "div"],
            [17, 45, 43, 44],
            [2, 46, "body"],
            [6, 47, 41, 46],
            [2, 48, "appendChild"],
            [24, 49, 47, 48],
            [17, 50, 49, 45]
        ]);
        let mut program = prefix.as_array().unwrap().clone();
        program.extend(
            json!([
                [2, 51, "parentNode"],
                [6, 52, 45, 51],
                [20, 52, 47, 2, 53, "attached"],
                [2, 54, "remove"],
                [24, 55, 45, 54],
                [17, 56, 55],
                [6, 57, 45, 51],
                [2, 58, null],
                [20, 57, 58, 2, 59, "detached"],
                [2, 60, []],
                [5, 60, 53],
                [5, 60, 59],
                [15, 61, 60],
                [7, 3, 61]
            ])
            .as_array()
            .unwrap()
            .iter()
            .cloned(),
        );
        assert_eq!(
            solve(
                &encode(json!(program), "key"),
                "key",
                &profile(),
                CancellationToken::new()
            ),
            Ok(BASE64_STANDARD.encode(r#"["attached","detached"]"#))
        );
        for property in ["offsetWidth", "innerText"] {
            let mut program = prefix.as_array().unwrap().clone();
            program.extend([
                json!([2, 51, property]),
                json!([6, 52, 45, 51]),
                json!([7, 3, 52]),
            ]);
            assert_eq!(
                solve(
                    &encode(json!(program), "key"),
                    "key",
                    &profile(),
                    CancellationToken::new()
                ),
                Err(Error::UnsupportedDomMeasurement {
                    element: "DIV",
                    property,
                    connected: true
                })
            );
        }
        let mut program = prefix.as_array().unwrap().clone();
        program.extend([
            json!([2, 51, "getBoundingClientRect"]),
            json!([6, 52, 45, 51]),
            json!([7, 3, 52]),
        ]);
        assert_eq!(
            solve(
                &encode(json!(program), "key"),
                "key",
                &profile(),
                CancellationToken::new()
            ),
            Ok(BASE64_STANDARD.encode("function getBoundingClientRect() { [native code] }"))
        );
        let mut program = prefix.as_array().unwrap().clone();
        program.extend([
            json!([2, 51, "getBoundingClientRect"]),
            json!([24, 52, 45, 51]),
            json!([17, 53, 52]),
        ]);
        assert_eq!(
            solve(
                &encode(json!(program), "key"),
                "key",
                &profile(),
                CancellationToken::new()
            ),
            Err(Error::UnsupportedNativeLayout {
                reason: "body_flow"
            })
        );
    }

    #[test]
    fn detached_element_reflections_preserve_literal_text_and_readonly_metadata() {
        for (property, value, expected, success) in [
            ("id", json!("fixture"), "fixture", true),
            ("id", Json::Null, "null", true),
            ("className", json!("a b"), "a b", true),
            ("ariaHidden", json!(true), "true", true),
            ("ariaHidden", json!(false), "false", true),
            ("ariaHidden", Json::Null, "null", true),
            (
                "textContent",
                json!("<b>literal</b>"),
                "<b>literal</b>",
                true,
            ),
            ("textContent", Json::Null, "", true),
            ("innerText", json!("a\r\nb"), "ab", true),
            ("innerText", json!("<b>literal</b>"), "<b>literal</b>", true),
            ("innerText", Json::Null, "", true),
            ("nodeName", json!("SPAN"), "DIV", false),
        ] {
            let program = json!([
                [2, 40, "document"],
                [6, 41, 10, 40],
                [2, 42, "createElement"],
                [24, 43, 41, 42],
                [2, 44, "div"],
                [17, 45, 43, 44],
                [8, 46, 45],
                [2, 47, "Reflect"],
                [6, 48, 10, 47],
                [2, 49, "set"],
                [24, 50, 48, 49],
                [2, 51, property],
                [2, 52, value],
                [17, 53, 50, 45, 51, 52],
                [6, 54, 46, 51],
                [2, 55, []],
                [5, 55, 53],
                [5, 55, 54],
                [15, 56, 55],
                [7, 3, 56]
            ]);
            assert_eq!(
                solve(
                    &encode(program, "key"),
                    "key",
                    &profile(),
                    CancellationToken::new()
                ),
                Ok(BASE64_STANDARD.encode(
                    json!([
                        success,
                        if property == "ariaHidden" && expected == "null" {
                            Json::Null
                        } else {
                            json!(expected)
                        }
                    ])
                    .to_string()
                ))
            );
        }
    }

    #[test]
    fn number_predicates_distinguish_coercing_globals_from_static_methods() {
        let profile = profile();
        let mut vm = Vm::new(&profile, "key", CancellationToken::new(), Limits::default()).unwrap();
        assert!(matches!(
            vm.call(Value::Host(Host::Number), &[], 0).unwrap(),
            Value::Number(0.0)
        ));
        assert!(
            matches!(vm.call(Value::Host(Host::Number), &[Value::Undefined], 0).unwrap(), Value::Number(n) if n.is_nan())
        );
        let numeric = vm.text("12".into()).unwrap();
        let text = vm.text("text".into()).unwrap();
        for (f, argument, expected) in [
            (Builtin::IsFinite, numeric, true),
            (Builtin::NumberIsFinite, numeric, false),
            (Builtin::IsNan, text, true),
            (Builtin::NumberIsNan, text, false),
            (Builtin::NumberIsNan, Value::Number(f64::NAN), true),
            (Builtin::NumberIsFinite, Value::Number(f64::INFINITY), false),
            (Builtin::NumberIsInteger, Value::Number(-0.0), true),
            (Builtin::NumberIsInteger, Value::Number(1.5), false),
            (
                Builtin::NumberIsSafeInteger,
                Value::Number(9_007_199_254_740_991.0),
                true,
            ),
            (
                Builtin::NumberIsSafeInteger,
                Value::Number(9_007_199_254_740_992.0),
                false,
            ),
        ] {
            assert!(
                matches!(vm.builtin(f, &[argument], 0).unwrap(), Value::Bool(value) if value == expected)
            );
        }
    }

    #[test]
    fn square_root_preserves_numeric_edge_cases() {
        let profile = profile();
        let mut vm = Vm::new(&profile, "key", CancellationToken::new(), Limits::default()).unwrap();
        for (input, expected) in [
            (4.0, 2.0),
            (2.0, std::f64::consts::SQRT_2),
            (0.0, 0.0),
            (-0.0, -0.0),
            (f64::INFINITY, f64::INFINITY),
        ] {
            let Value::Number(result) = vm
                .builtin(Builtin::Sqrt, &[Value::Number(input)], 0)
                .unwrap()
            else {
                panic!("expected numeric sqrt");
            };
            assert_eq!(result.to_bits(), expected.to_bits());
        }
        for args in [vec![], vec![Value::Number(-1.0)], vec![Value::Undefined]] {
            assert!(
                matches!(vm.builtin(Builtin::Sqrt, &args, 0).unwrap(), Value::Number(n) if n.is_nan())
            );
        }
        let text = vm.text("9".into()).unwrap();
        assert!(matches!(
            vm.builtin(Builtin::Sqrt, &[text], 0).unwrap(),
            Value::Number(3.0)
        ));
    }

    #[test]
    fn native_weak_set_tracks_object_identity_without_enumerating_keys() {
        let profile = profile();
        let mut vm = Vm::new(&profile, "key", CancellationToken::new(), Limits::default()).unwrap();
        let object = vm.alloc(Heap::Object(Vec::new()), 0).unwrap();
        let other = vm.alloc(Heap::Object(Vec::new()), 0).unwrap();
        let iterable = vm.array(vec![object, object]).unwrap();
        let args = vm.array(vec![iterable]).unwrap();
        let set = vm
            .builtin(
                Builtin::Construct,
                &[Value::Host(Host::WeakSetConstructor), args],
                0,
            )
            .unwrap();
        for (key, expected) in [(object, true), (other, false), (Value::Number(1.0), false)] {
            assert!(
                matches!(vm.weak_set(Builtin::WeakSetHas, set, key, 0).unwrap(), Value::Bool(value) if value == expected)
            );
        }
        let same = vm.weak_set(Builtin::WeakSetAdd, set, set, 0).unwrap();
        assert!(vm.equal(set, same).unwrap());
        assert!(matches!(
            vm.weak_set(Builtin::WeakSetDelete, set, object, 0).unwrap(),
            Value::Bool(true)
        ));
        assert!(matches!(
            vm.weak_set(Builtin::WeakSetDelete, set, object, 0).unwrap(),
            Value::Bool(false)
        ));
        assert_eq!(vm.json(set, 0).unwrap(), json!({}));
        assert_eq!(vm.string(set, 0).unwrap(), "[object WeakSet]");
        let keys = vm.builtin(Builtin::Keys, &[set], 0).unwrap();
        assert_eq!(vm.json(keys, 0).unwrap(), json!([]));
        assert!(matches!(
            vm.weak_set(Builtin::WeakSetAdd, set, Value::Null, 0),
            Err(Error::GuestException)
        ));
        assert!(matches!(
            vm.weak_set(Builtin::WeakSetHas, other, object, 0),
            Err(Error::UnsupportedReflectionApi)
        ));
        assert!(matches!(
            vm.builtin(
                Builtin::Construct,
                &[
                    Value::Host(Host::WeakSetConstructor),
                    args,
                    Value::Host(Host::Object)
                ],
                0
            ),
            Err(Error::UnsupportedReflectionApi)
        ));
        vm.budget.limits.allocation_bytes = vm.budget.allocated;
        assert!(matches!(
            vm.weak_set(Builtin::WeakSetAdd, set, other, 0),
            Err(Error::AllocationLimit)
        ));
    }

    #[test]
    fn window_input_handler_attributes_retain_objects_and_clear_primitives() {
        let profile = profile();
        let mut vm = Vm::new(&profile, "key", CancellationToken::new(), Limits::default()).unwrap();
        let window = Value::Host(Host::Window);
        let object = vm.alloc(Heap::Object(Vec::new()), 0).unwrap();
        let text = vm.text("literal".into()).unwrap();
        for name in WINDOW_HANDLERS {
            let key = vm.text((*name).into()).unwrap();
            assert!(matches!(vm.property(window, key, 0).unwrap(), Value::Null));
            for value in [
                object,
                Value::Builtin(Builtin::Now),
                Value::Null,
                Value::Undefined,
                Value::Bool(false),
                Value::Number(1.0),
                text,
            ] {
                vm.builtin(Builtin::Set, &[window, key, value], 0).unwrap();
                let result = vm.property(window, key, 0).unwrap();
                let expected = if vm.equal(value, object).unwrap() || vm.is_callable(value) {
                    value
                } else {
                    Value::Null
                };
                assert!(vm.equal(result, expected).unwrap());
            }
        }
    }

    #[test]
    fn function_bind_preserves_prefix_order_receiver_and_aliased_mutation() {
        let profile = profile();
        let mut vm = Vm::new(&profile, "key", CancellationToken::new(), Limits::default()).unwrap();
        let binder = vm
            .alloc(
                Heap::Bound {
                    function: Value::Builtin(Builtin::Bind),
                    receiver: Value::Builtin(Builtin::Hypot),
                    arguments: Vec::new(),
                },
                0,
            )
            .unwrap();
        let hypot3 = vm
            .call(binder, &[Value::Null, Value::Number(3.0)], 0)
            .unwrap();
        let result = vm.call(hypot3, &[Value::Number(4.0)], 0).unwrap();
        assert!(vm.equal(result, Value::Number(5.0)).unwrap());

        let array = vm
            .array(vec![Value::Number(1.0), Value::Number(2.0)])
            .unwrap();
        let other = vm.array(vec![Value::Number(9.0)]).unwrap();
        let binder = vm
            .alloc(
                Heap::Bound {
                    function: Value::Builtin(Builtin::Bind),
                    receiver: Value::Builtin(Builtin::ArrayPop),
                    arguments: Vec::new(),
                },
                0,
            )
            .unwrap();
        let first = vm.call(binder, &[array], 0).unwrap();
        let rebinder = vm
            .alloc(
                Heap::Bound {
                    function: Value::Builtin(Builtin::Bind),
                    receiver: first,
                    arguments: Vec::new(),
                },
                0,
            )
            .unwrap();
        let rebound = vm.call(rebinder, &[other], 0).unwrap();
        let result = vm.call(rebound, &[], 0).unwrap();
        assert!(vm.equal(result, Value::Number(2.0)).unwrap());
        assert_eq!(vm.json(array, 0).unwrap(), json!([1]));
        assert_eq!(vm.json(other, 0).unwrap(), json!([9]));

        // Exercise the actual bytecode property lookup and bind route.
        let program = json!([
            [2, 40, "Reflect"],
            [6, 41, 10, 40],
            [2, 42, "set"],
            [6, 43, 41, 42],
            [2, 44, "bind"],
            [24, 45, 43, 44],
            [2, 46, null],
            [2, 47, {}],
            [2, 48, "field"],
            [17, 49, 45, 46, 47, 48],
            [2, 50, "owned"],
            [17, 51, 49, 50],
            [6, 52, 47, 48],
            [7, 3, 52]
        ]);
        assert_eq!(
            solve(
                &encode(program, "key"),
                "key",
                &profile,
                CancellationToken::new()
            ),
            Ok(BASE64_STANDARD.encode("owned"))
        );
        vm.budget.limits.allocation_bytes = vm.budget.allocated;
        assert!(matches!(
            vm.call(hypot3, &[Value::Number(4.0)], 0),
            Err(Error::AllocationLimit)
        ));
    }

    #[test]
    fn native_environment_has_no_webdriver_session() {
        assert_eq!(
            solve(
                &encode(
                    json!([
                        [2, 40, "navigator"],
                        [6, 41, 10, 40],
                        [2, 42, "webdriver"],
                        [6, 43, 41, 42],
                        [7, 3, 43]
                    ]),
                    "key"
                ),
                "key",
                &profile(),
                CancellationToken::new()
            ),
            Ok(BASE64_STANDARD.encode("false"))
        );
    }

    #[test]
    fn relational_comparison_uses_utf16_for_two_strings_and_numbers_otherwise() {
        for (left, right, expected) in [
            (json!("10"), json!("2"), true),
            (json!("10"), json!(2), false),
            (json!("a"), json!("aa"), true),
            (json!("😀"), json!("\u{e000}"), true),
            (json!("same"), json!("same"), false),
            (json!("not a number"), json!(2), false),
            (Json::Null, json!(true), true),
        ] {
            assert_eq!(
                solve(
                    &encode(
                        json!([[2, 40, left], [2, 41, right], [29, 42, 40, 41], [7, 3, 42]]),
                        "key"
                    ),
                    "key",
                    &profile(),
                    CancellationToken::new()
                ),
                Ok(BASE64_STANDARD.encode(expected.to_string()))
            );
        }
        // Fixed-point Rust formatting here would differ from JavaScript's
        // shortest representation. Reject until that conversion is supported.
        assert_eq!(
            number_string(1_000_000_000_000_000_100.0),
            Err(Error::UnsupportedString)
        );
    }

    #[test]
    fn observer_globals_are_guest_owned_scoped_and_shared_with_snapshot() {
        let collector = encode(
            json!([
                [2, 40, "__oai_so_h"],
                [6, 41, 10, 40],
                [23, 41, 4, "unexpected previous handler"],
                [2, 42, "Reflect"],
                [6, 43, 10, 42],
                [2, 44, "set"],
                [24, 45, 43, 44],
                [30, 46, 47, [[2, 47, "native callback"]]],
                [17, 48, 45, 10, 40, 46],
                [3, "ready"]
            ]),
            "key",
        );
        let profile = Arc::new(profile());
        let observer =
            Observer::start(&collector, "key", profile.clone(), CancellationToken::new()).unwrap();
        let snapshot = encode(
            json!([
                [2, 50, "__oai_so_h"],
                [6, 51, 10, 50],
                [17, 52, 51],
                [7, 3, 52]
            ]),
            "key",
        );
        assert_eq!(
            observer.snapshot(&snapshot),
            Ok(BASE64_STANDARD.encode("native callback"))
        );
        // A fresh window on the same profile has no previous handler.
        assert!(
            Observer::start(&collector, "key", profile.clone(), CancellationToken::new()).is_ok()
        );
        assert_eq!(
            solve(&collector, "key", &profile, CancellationToken::new()),
            Err(Error::UnsupportedSentinelSdkState)
        );
        let mut vm = Vm::new(profile, "key", CancellationToken::new(), Limits::default()).unwrap();
        vm.enable_observer_globals().unwrap();
        let arbitrary = vm.text("other_private_state".into()).unwrap();
        assert!(matches!(
            vm.builtin(
                Builtin::Set,
                &[Value::Host(Host::Window), arbitrary, Value::Bool(true)],
                0
            ),
            Err(Error::UnsupportedHostMutation)
        ));
    }

    #[test]
    fn observer_snapshot_preserves_collector_state_and_mutated_key() {
        let collector = encode(
            json!([
                [2, 50, "before"],
                [3, "collector-complete"],
                [2, 50, "after"],
                [2, 16, "snapshot-key"]
            ]),
            "collector-key",
        );
        let observer = Observer::start(
            &collector,
            "collector-key",
            Arc::new(profile()),
            CancellationToken::new(),
        )
        .unwrap();
        let snapshot = encode(json!([[7, 3, 50]]), "snapshot-key");
        assert_eq!(
            observer.snapshot(&snapshot),
            Ok(BASE64_STANDARD.encode("after"))
        );
        let observer = Observer::start(
            &encode(json!([[3, "ready"]]), "key"),
            "key",
            Arc::new(profile()),
            CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(
            observer.snapshot(&encode(json!([[7, 3, 50]]), "key")),
            Ok(BASE64_STANDARD.encode("undefined"))
        );
    }

    #[test]
    fn observer_cancellation_and_shared_budget_apply_to_snapshot() {
        let cancel = CancellationToken::new();
        let observer = Observer::start(
            &encode(json!([[3, "ready"]]), "key"),
            "key",
            Arc::new(profile()),
            cancel.clone(),
        )
        .unwrap();
        cancel.cancel();
        assert_eq!(
            observer.snapshot(&encode(json!([[3, "snapshot"]]), "key")),
            Err(Error::Cancelled)
        );

        let profile = profile();
        let mut vm = Vm::new(&profile, "key", CancellationToken::new(), Limits::default()).unwrap();
        let dx = encode(json!([[3, "ready"]]), "key");
        vm.execute_encoded(&dx).unwrap();
        vm.budget.limits.allocation_bytes = vm.budget.allocated;
        assert_eq!(vm.execute_encoded(&dx), Err(Error::AllocationLimit));
    }

    #[test]
    fn observer_network_wait_does_not_consume_execution_budget() {
        let mut observer = Observer::start(
            &encode(json!([[3, "ready"]]), "key"),
            "key",
            Arc::new(profile()),
            CancellationToken::new(),
        )
        .unwrap();
        // Simulate a network wait longer than the execution limit without
        // sleeping. Only the paused clock changes; consumed work is retained.
        observer.vm.budget.started -= std::time::Duration::from_secs(10);
        assert_eq!(
            observer.snapshot(&encode(json!([[3, "snapshot"]]), "key")),
            Ok(BASE64_STANDARD.encode("snapshot"))
        );
    }

    #[test]
    fn observer_snapshot_keeps_cumulative_execution_and_instruction_limits() {
        for time_limit in [false, true] {
            let mut observer = Observer::start(
                &encode(json!([[3, "ready"]]), "key"),
                "key",
                Arc::new(profile()),
                CancellationToken::new(),
            )
            .unwrap();
            if time_limit {
                observer.vm.budget.elapsed_before_pause = observer.vm.budget.limits.elapsed;
            } else {
                observer.vm.budget.limits.instructions = observer.vm.budget.steps;
            }
            assert_eq!(
                observer.snapshot(&encode(json!([[3, "snapshot"]]), "key")),
                Err(if time_limit {
                    Error::Deadline
                } else {
                    Error::InstructionLimit
                })
            );
        }
    }

    #[test]
    fn router_metadata_uses_observed_values_and_preserves_object_identity() {
        let html = br#"<html data-build="fixture"><script src="/app.js"></script><script>window.__reactRouterContext = {"ssr":true,"isSpaMode":false,"future":{"v8_middleware":false}};</script>"#;
        let profile = Profile::new(Bootstrap::parse(html, "https://chatgpt.com").unwrap());
        let program = json!([
            [2, 40, "__reactRouterContext"],
            [6, 41, 10, 40],
            [2, 42, "future"],
            [6, 43, 41, 42],
            [6, 44, 41, 42],
            [20, 43, 44, 3, "same"],
            [4, "wrong"]
        ]);
        assert_eq!(
            solve(
                &encode(program, "key"),
                "key",
                &profile,
                CancellationToken::new()
            ),
            Ok(BASE64_STANDARD.encode("same"))
        );
        for (field, expected) in [
            ("isSpaMode", Ok(BASE64_STANDARD.encode("false"))),
            ("loaderData", Err(Error::UnsupportedRouterSessionState)),
            ("notObserved", Err(Error::UnsupportedRouterField)),
        ] {
            let program = json!([
                [2, 40, "__reactRouterContext"],
                [6, 41, 10, 40],
                [2, 42, field],
                [6, 43, 41, 42],
                [7, 3, 43]
            ]);
            assert_eq!(
                solve(
                    &encode(program, "key"),
                    "key",
                    &profile,
                    CancellationToken::new()
                ),
                expected
            );
        }
    }

    #[test]
    fn absent_router_global_allows_presence_checks_but_never_required_values() {
        let missing = profile();
        for tail in [
            json!([[23, 41, 4, "unexpected presence"], [3, "absent"]]),
            json!([[20, 41, 99, 3, "absent"], [4, "wrong equality"]]),
        ] {
            let mut instructions = vec![
                json!([2, 40, "__reactRouterContext"]),
                json!([6, 41, 10, 40]),
            ];
            instructions.extend(tail.as_array().unwrap().iter().cloned());
            assert_eq!(
                solve(
                    &encode(Json::Array(instructions), "key"),
                    "key",
                    &missing,
                    CancellationToken::new()
                ),
                Ok(BASE64_STANDARD.encode("absent"))
            );
        }
        // Copying through an alias cannot discard the unavailable-value marker.
        for (tail, failure) in [
            (
                json!([[2, 43, "loaderData"], [6, 44, 42, 43]]),
                Error::RequiredRouterContextProperty,
            ),
            (json!([[7, 42]]), Error::RequiredRouterContextCall),
            (json!([[7, 3, 42]]), Error::RequiredRouterContextCoercion),
            (
                json!([[15, 43, 42]]),
                Error::RequiredRouterContextSerialization,
            ),
            (
                json!([[2, 43, []], [5, 43, 42], [15, 44, 43]]),
                Error::RequiredRouterContextSerialization,
            ),
            (
                json!([[2, 43, 1], [27, 43, 42]]),
                Error::RequiredRouterContextUnavailable,
            ),
        ] {
            let mut instructions = vec![
                json!([2, 40, "__reactRouterContext"]),
                json!([6, 41, 10, 40]),
                json!([8, 42, 41]),
            ];
            instructions.extend(tail.as_array().unwrap().iter().cloned());
            instructions.push(json!([3, "must-not-complete"]));
            assert_eq!(
                solve(
                    &encode(Json::Array(instructions), "key"),
                    "key",
                    &missing,
                    CancellationToken::new()
                ),
                Err(failure)
            );
        }
    }

    #[test]
    fn malformed_router_metadata_is_not_reported_as_absent() {
        let html = br#"<html data-build="fixture"><script src="/app.js"></script><script>window.__reactRouterContext = loadContext();</script>"#;
        let profile = Profile::new(Bootstrap::parse(html, "https://chatgpt.com").unwrap());
        let program = json!([
            [2, 40, "__reactRouterContext"],
            [6, 41, 10, 40],
            [3, "must-not-complete"]
        ]);
        assert_eq!(
            solve(
                &encode(program, "key"),
                "key",
                &profile,
                CancellationToken::new()
            ),
            Err(Error::InvalidRouterMetadata)
        );
    }

    #[test]
    fn unmatched_script_lookup_is_null_and_presence_checks_only_skip_undefined() {
        // Synthetic programs also executed against the official public runtime.
        // See evidence/official-runtime-contracts-2026-10-12.json.
        let prefix = vec![json!([2, 40, "missing-script"]), json!([11, 41, 40])];
        for (tail, expected) in [
            (json!([[15, 42, 41], [7, 3, 42]]), "null"),
            (json!([[7, 3, 41]]), "null"),
            (json!([[2, 42, null], [20, 41, 42, 3, "equal"]]), "equal"),
            (
                json!([[2, 43, "skipped"], [23, 41, 2, 43, "called"], [7, 3, 43]]),
                "called",
            ),
            (
                json!([[2, 43, "skipped"], [23, 99, 2, 43, "called"], [7, 3, 43]]),
                "skipped",
            ),
        ] {
            let mut instructions = prefix.clone();
            instructions.extend(tail.as_array().unwrap().iter().cloned());
            assert_eq!(
                solve(
                    &encode(Json::Array(instructions), "key"),
                    "key",
                    &profile(),
                    CancellationToken::new()
                ),
                Ok(BASE64_STANDARD.encode(expected))
            );
        }
        let program = json!([
            [2, 40, "missing-script"],
            [11, 41, 40],
            [2, 42, "length"],
            [6, 43, 41, 42]
        ]);
        assert_eq!(
            solve(
                &encode(program, "key"),
                "key",
                &profile(),
                CancellationToken::new()
            ),
            Err(Error::RequiredNullProperty)
        );
    }

    #[test]
    fn explicit_guest_try_can_handle_a_known_absent_global_type_error() {
        let prefix = vec![
            json!([2, 40, "__reactRouterContext"]),
            json!([6, 41, 10, 40]),
            json!([2, 42, "loaderData"]),
        ];
        for tail in [
            json!([[13, 43, 6, 44, 41, 42], [7, 3, 43]]),
            json!([[22, 43, [[6, 44, 41, 42]]], [7, 3, 43]]),
            json!([
                [30, 44, 45, [], [[6, 45, 41, 42]]],
                [17, 43, 44],
                [7, 3, 43]
            ]),
        ] {
            let mut instructions = prefix.clone();
            instructions.extend(tail.as_array().unwrap().iter().cloned());
            assert_eq!(
                solve(
                    &encode(Json::Array(instructions), "key"),
                    "key",
                    &profile(),
                    CancellationToken::new()
                ),
                Ok(BASE64_STANDARD.encode(
                    "TypeError: Cannot read properties of undefined (reading 'loaderData')"
                ))
            );
        }
        // Catching an exception is not itself a successful VM result.
        let mut instructions = prefix;
        instructions.push(json!([22, 43, [[6, 44, 41, 42]]]));
        assert_eq!(
            solve(
                &encode(Json::Array(instructions), "key"),
                "key",
                &profile(),
                CancellationToken::new()
            ),
            Err(Error::MissingResult)
        );
    }

    #[test]
    fn guest_try_never_converts_unsupported_capabilities_or_unknown_instructions() {
        for (program, expected) in [
            (
                json!([[13, 40, 99], [3, "must-not-complete"]]),
                Error::UnknownInstruction,
            ),
            (
                json!([[22, 40, [[99]]], [3, "must-not-complete"]]),
                Error::UnknownInstruction,
            ),
            (
                json!([[17, 40, 99], [3, "must-not-complete"]]),
                Error::UnknownInstruction,
            ),
            (
                json!([
                    [22, 40, [[2, 41, "Function"], [6, 42, 10, 41]]],
                    [3, "must-not-complete"]
                ]),
                Error::UnsupportedFunctionApi,
            ),
            (
                json!([
                    [22, 40, [[30, 41, 42, [], [[7, 41]]], [7, 41]]],
                    [3, "must-not-complete"]
                ]),
                Error::NestingLimit,
            ),
        ] {
            assert_eq!(
                solve(
                    &encode(program, "key"),
                    "key",
                    &profile(),
                    CancellationToken::new()
                ),
                Err(expected)
            );
        }
    }

    #[test]
    fn explicit_guest_try_handles_nullish_properties_without_hiding_uncaught_failures() {
        for (prefix, kind, failure) in [
            (
                vec![json!([2, 40, null])],
                "null",
                Error::RequiredNullProperty,
            ),
            (vec![], "undefined", Error::RequiredUndefinedProperty),
        ] {
            for catch in [false, true] {
                let mut instructions = prefix.clone();
                instructions.push(json!([2, 41, "length"]));
                if catch {
                    instructions.push(json!([22, 43, [[6, 42, 40, 41]]]));
                    instructions.push(json!([7, 3, 43]));
                } else {
                    instructions.push(json!([6, 42, 40, 41]));
                    instructions.push(json!([3, "must-not-complete"]));
                }
                assert_eq!(
                    solve(
                        &encode(Json::Array(instructions), "key"),
                        "key",
                        &profile(),
                        CancellationToken::new()
                    ),
                    if catch {
                        Ok(BASE64_STANDARD.encode(format!(
                            "TypeError: Cannot read properties of {kind} (reading 'length')"
                        )))
                    } else {
                        Err(failure)
                    }
                );
            }
        }
    }

    #[test]
    fn executes_register_aliases_and_literal_operands() {
        let program = json!([[8, 40, 2], [40, 41, "fixture"], [7, 3, 41]]);
        assert_eq!(
            solve(
                &encode(program, "key"),
                "key",
                &profile(),
                CancellationToken::new()
            ),
            Ok(BASE64_STANDARD.encode("fixture"))
        );
    }

    #[test]
    fn exact_requirements_key_is_required() {
        let dx = encode(json!([[3, "fixture"]]), "requirements");
        assert!(solve(&dx, "proof-token", &profile(), CancellationToken::new()).is_err());
    }

    #[test]
    fn register_keys_preserve_type_and_large_numeric_values() {
        let program = json!([
            [2, 9_876_543_210_u64, "numeric"],
            [2, "9876543210", "string"],
            [12, 40],
            [2, 41, 9_876_543_210_u64],
            [6, 42, 40, 41],
            [7, 3, 42]
        ]);
        assert_eq!(
            solve(
                &encode(program, "key"),
                "key",
                &profile(),
                CancellationToken::new()
            ),
            Ok(BASE64_STANDARD.encode("numeric"))
        );
    }

    #[test]
    fn host_and_callable_aliases_preserve_identity() {
        for program in [
            json!([
                [2, 40, "self"],
                [6, 41, 10, 40],
                [20, 41, 10, 3, "same"],
                [4, "wrong"]
            ]),
            json!([[8, 40, 3], [20, 40, 3, 3, "same"], [4, "wrong"]]),
        ] {
            assert_eq!(
                solve(
                    &encode(program, "key"),
                    "key",
                    &profile(),
                    CancellationToken::new()
                ),
                Ok(BASE64_STANDARD.encode("same"))
            );
        }
    }

    #[test]
    fn observed_array_removal_mutates_aliases_and_removes_only_one_entry() {
        for (value, expected) in [(1, "[2,1]"), (9, "[1,2]")] {
            let program = json!([
                [2, 40, [1, 2, 1]],
                [8, 41, 40],
                [2, 42, value],
                [27, 40, 42],
                [15, 43, 41],
                [7, 3, 43]
            ]);
            assert_eq!(
                solve(
                    &encode(program, "key"),
                    "key",
                    &profile(),
                    CancellationToken::new()
                ),
                Ok(BASE64_STANDARD.encode(expected))
            );
        }
    }

    #[test]
    fn known_noops_and_first_resolution_do_not_hide_later_unsupported_instructions() {
        let program = json!([[25], [26], [28], [3, "first"], [3, "second"]]);
        assert_eq!(
            solve(
                &encode(program, "key"),
                "key",
                &profile(),
                CancellationToken::new()
            ),
            Ok(BASE64_STANDARD.encode("first"))
        );
        let program = json!([[3, "first"], [999]]);
        assert_eq!(
            solve(
                &encode(program, "key"),
                "key",
                &profile(),
                CancellationToken::new()
            ),
            Err(Error::UnknownInstruction)
        );
    }

    #[test]
    fn property_indices_are_canonical_and_unimplemented_prototypes_stop() {
        for (key, expected) in [
            ("1", Ok(BASE64_STANDARD.encode("b"))),
            ("01", Ok(BASE64_STANDARD.encode("undefined"))),
            ("+1", Ok(BASE64_STANDARD.encode("undefined"))),
            (
                "push",
                Err(Error::UnsupportedPrototypeProperty {
                    value: "array",
                    property: "push",
                }),
            ),
        ] {
            let program = json!([
                [2, 40, ["a", "b"]],
                [2, 41, key],
                [6, 42, 40, 41],
                [7, 3, 42]
            ]);
            assert_eq!(
                solve(
                    &encode(program, "key"),
                    "key",
                    &profile(),
                    CancellationToken::new()
                ),
                expected
            );
        }
    }

    #[test]
    fn decode_failures_report_only_the_structural_stage() {
        for (dx, expected) in [
            ("!".to_owned(), Error::InvalidProgramEncoding),
            (
                btoa(&xor("not JSON", "key").unwrap()).unwrap(),
                Error::InvalidProgramJson,
            ),
            (
                encode(json!({"private": "value"}), "key"),
                Error::InvalidProgramShape,
            ),
        ] {
            assert_eq!(
                solve(&dx, "key", &profile(), CancellationToken::new()),
                Err(expected)
            );
        }
    }

    #[test]
    fn unknown_instructions_and_environment_never_produce_error_tokens() {
        for (program, expected) in [
            (
                json!([[99], [3, "fake-success"]]),
                Error::UnknownInstruction,
            ),
            (
                json!([
                    [2, 40, "navigator"],
                    [6, 41, 10, 40],
                    [2, 42, "geolocation"],
                    [6, 43, 41, 42],
                    [3, "fake-success"]
                ]),
                Error::UnsupportedGeolocation,
            ),
            (json!([[4, "private error"]]), Error::Rejected),
            (json!([[2, 40, "unused"]]), Error::MissingResult),
            (json!([[]]), Error::InvalidArguments),
        ] {
            assert_eq!(
                solve(
                    &encode(program, "key"),
                    "key",
                    &profile(),
                    CancellationToken::new()
                ),
                Err(expected)
            );
        }
    }

    #[test]
    fn nested_callbacks_share_instruction_and_allocation_limits() {
        let program = json!([[30, 40, 41, [], [[7, 40]]], [7, 40]]);
        assert_eq!(
            solve(
                &encode(program, "key"),
                "key",
                &profile(),
                CancellationToken::new()
            ),
            Err(Error::NestingLimit)
        );
        let program = json!([[2, 40, "large"], [3, "result"]]);
        let limits = Limits {
            allocation_bytes: 10,
            ..Limits::default()
        };
        assert_eq!(
            solve_with_limits(
                &encode(program, "key"),
                "key",
                &profile(),
                CancellationToken::new(),
                limits
            ),
            Err(Error::AllocationLimit)
        );
    }

    #[test]
    fn instruction_budget_exhaustion_stops_before_a_success_instruction() {
        let mut program = vec![json!([2, 40, 1]); 128];
        program.push(json!([3, "must-not-complete"]));
        assert_eq!(
            solve_with_limits(
                &encode(Json::Array(program), "key"),
                "key",
                &profile(),
                CancellationToken::new(),
                Limits {
                    instructions: 600,
                    ..Limits::default()
                }
            ),
            Err(Error::InstructionLimit)
        );
    }

    #[test]
    fn unsupported_dom_creation_reports_only_an_allowlisted_capability() {
        for (tag, expected) in [
            ("iframe", Error::UnsupportedFrameEnvironment),
            ("SCRIPT", Error::UnsupportedScriptExecution),
            ("private-unknown-value", Error::UnsupportedDomCreation),
        ] {
            let program = json!([
                [2, 40, "document"],
                [6, 41, 10, 40],
                [2, 42, "createElement"],
                [24, 43, 41, 42],
                [2, 44, tag],
                [17, 45, 43, 44],
                [3, "must-not-complete"]
            ]);
            assert_eq!(
                solve(
                    &encode(program, "key"),
                    "key",
                    &profile(),
                    CancellationToken::new()
                ),
                Err(expected)
            );
        }
    }

    #[test]
    fn native_elements_expose_metadata_and_stop_on_layout_or_styling() {
        for (property, expected) in [
            ("tagName", Ok(BASE64_STANDARD.encode("DIV"))),
            ("nodeType", Ok(BASE64_STANDARD.encode("1"))),
            ("id", Ok(String::new())),
            (
                "offsetHeight",
                Err(Error::UnsupportedDomMeasurement {
                    element: "DIV",
                    property: "offsetHeight",
                    connected: false,
                }),
            ),
            ("appendChild", Err(Error::UnsupportedDomElement)),
        ] {
            let program = json!([
                [2, 40, "document"],
                [6, 41, 10, 40],
                [2, 42, "createElement"],
                [24, 43, 41, 42],
                [2, 44, "div"],
                [17, 45, 43, 44],
                [2, 46, property],
                [6, 47, 45, 46],
                [2, 48, "metadata:"],
                [5, 48, 47],
                [7, 3, 48]
            ]);
            let expected = expected.map(|s| {
                let mut raw = b"metadata:".to_vec();
                raw.extend(BASE64_STANDARD.decode(s).unwrap());
                BASE64_STANDARD.encode(raw)
            });
            assert_eq!(
                solve(
                    &encode(program, "key"),
                    "key",
                    &profile(),
                    CancellationToken::new()
                ),
                expected
            );
        }
    }

    #[test]
    fn detached_element_style_has_stable_identity_and_no_computed_values() {
        let prefix = vec![
            json!([2, 40, "document"]),
            json!([6, 41, 10, 40]),
            json!([2, 42, "createElement"]),
            json!([24, 43, 41, 42]),
            json!([2, 44, "div"]),
            json!([17, 45, 43, 44]),
            json!([2, 46, "style"]),
            json!([6, 47, 45, 46]),
        ];
        for (tail, expected) in [
            (
                json!([[6, 48, 45, 46], [20, 47, 48, 3, "same"]]),
                Ok(BASE64_STANDARD.encode("same")),
            ),
            (
                json!([[2, 48, "length"], [6, 49, 47, 48], [7, 3, 49]]),
                Ok(BASE64_STANDARD.encode("0")),
            ),
            (
                json!([[2, 48, "getPropertyValue"], [6, 49, 47, 48]]),
                Err(Error::UnsupportedStyleProperty),
            ),
            (
                json!([[15, 48, 47]]),
                Err(Error::UnsupportedStyleSerialization),
            ),
            (
                json!([
                    [2, 48, "Reflect"],
                    [6, 49, 10, 48],
                    [2, 50, "set"],
                    [6, 51, 49, 50],
                    [2, 52, "width"],
                    [2, 53, "100px"],
                    [17, 54, 51, 47, 52, 53],
                    [2, 55, "cssText"],
                    [6, 56, 47, 55],
                    [7, 3, 56]
                ]),
                Ok(BASE64_STANDARD.encode("width: 100px;")),
            ),
            (
                json!([
                    [2, 48, "Reflect"],
                    [6, 49, 10, 48],
                    [2, 50, "set"],
                    [6, 51, 49, 50],
                    [2, 52, "width"],
                    [2, 53, "calc(100px + 1vw)"],
                    [22, 54, [[17, 55, 51, 47, 52, 53]]]
                ]),
                Err(Error::UnsupportedCssValue {
                    property: "width",
                    syntax: "function",
                }),
            ),
        ] {
            let mut instructions = prefix.clone();
            instructions.extend(tail.as_array().unwrap().iter().cloned());
            instructions.push(json!([3, "must-not-complete"]));
            assert_eq!(
                solve(
                    &encode(Json::Array(instructions), "key"),
                    "key",
                    &profile(),
                    CancellationToken::new()
                ),
                expected
            );
        }
    }

    #[test]
    fn cancellation_deadline_and_input_limits_precede_execution() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            solve("bad", "key", &profile(), cancel),
            Err(Error::Cancelled)
        );
        let limits = Limits {
            elapsed: std::time::Duration::ZERO,
            ..Limits::default()
        };
        assert_eq!(
            solve_with_limits("bad", "key", &profile(), CancellationToken::new(), limits),
            Err(Error::Deadline)
        );
        let limits = Limits {
            input_bytes: 2,
            ..Limits::default()
        };
        assert_eq!(
            solve_with_limits("bad", "key", &profile(), CancellationToken::new(), limits),
            Err(Error::InputLimit)
        );
    }

    #[test]
    fn latin1_base64_is_distinct_from_utf8() {
        assert_eq!(btoa("é"), Ok("6Q==".into()));
        assert_eq!(atob("6Q=="), Ok("é".into()));
        assert_eq!(btoa("😀"), Err(Error::UnsupportedString));
    }

    #[test]
    fn object_serialization_preserves_js_key_order_and_aliases() {
        let program = json!([[2,40,{"z":1,"2":2,"1":3,"a":4}],[8,41,40],[15,42,41],[7,3,42]]);
        assert_eq!(
            solve(
                &encode(program, "key"),
                "key",
                &profile(),
                CancellationToken::new()
            ),
            Ok(BASE64_STANDARD.encode(r#"{"1":3,"2":2,"z":1,"a":4}"#))
        );
    }

    #[test]
    fn native_storage_is_empty_then_retains_only_its_own_session_writes() {
        let profile = profile();
        let read = |kind: &str| {
            json!([
                [2, 40, kind],
                [6, 41, 10, 40],
                [2, 42, "getItem"],
                [6, 43, 41, 42],
                [2, 44, "fixture"],
                [17, 45, 43, 44],
                [7, 3, 45]
            ])
        };
        let run = |program| {
            solve(
                &encode(program, "key"),
                "key",
                &profile,
                CancellationToken::new(),
            )
        };
        assert_eq!(
            run(read("localStorage")),
            Ok(BASE64_STANDARD.encode("null"))
        );
        let write = json!([
            [2, 40, "localStorage"],
            [6, 41, 10, 40],
            [2, 42, "setItem"],
            [6, 43, 41, 42],
            [2, 44, "fixture"],
            [2, 45, "native-value"],
            [7, 43, 44, 45],
            [3, "written"]
        ]);
        assert!(run(write).is_ok());
        assert_eq!(
            run(read("localStorage")),
            Ok(BASE64_STANDARD.encode("native-value"))
        );
        assert_eq!(
            run(read("sessionStorage")),
            Ok(BASE64_STANDARD.encode("null"))
        );
    }

    #[test]
    fn call_and_set_estimates_the_actual_native_local_store() {
        let profile = profile();
        profile
            .storage(StorageKind::Local)
            .lock()
            .unwrap()
            .set("key".into(), "value".into())
            .unwrap();
        // Session storage is a separate shelf and is not counted here.
        profile
            .storage(StorageKind::Session)
            .lock()
            .unwrap()
            .set("other".into(), "value".into())
            .unwrap();
        let program = json!([
            [2, 40, "navigator"],
            [6, 41, 10, 40],
            [2, 42, "storage"],
            [6, 43, 41, 42],
            [2, 44, "estimate"],
            [24, 45, 43, 44],
            [17, 46, 45],
            [15, 47, 46],
            [7, 3, 47]
        ]);
        assert_eq!(
            solve(
                &encode(program, "key"),
                "key",
                &profile,
                CancellationToken::new()
            ),
            Ok(BASE64_STANDARD.encode(r#"{"usage":8,"quota":65536}"#))
        );
    }

    #[test]
    fn declared_screen_and_viewport_match_the_requirements_fingerprint() {
        let profile = profile();
        let program = json!([
            [2, 40, "screen"],
            [6, 41, 10, 40],
            [2, 42, "width"],
            [6, 43, 41, 42],
            [2, 44, "height"],
            [6, 45, 41, 44],
            [5, 43, 45],
            [7, 3, 43]
        ]);
        assert_eq!(
            solve(
                &encode(program, "key"),
                "key",
                &profile,
                CancellationToken::new()
            ),
            Ok(BASE64_STANDARD.encode(profile.config()[0].to_string()))
        );
        let program = json!([
            [2, 40, "innerWidth"],
            [6, 41, 10, 40],
            [2, 42, "innerHeight"],
            [6, 43, 10, 42],
            [5, 41, 43],
            [7, 3, 41]
        ]);
        assert_eq!(
            solve(
                &encode(program, "key"),
                "key",
                &profile,
                CancellationToken::new()
            ),
            Ok(BASE64_STANDARD.encode(profile.config()[0].to_string()))
        );
    }

    #[test]
    fn bound_methods_have_distinct_identity_and_native_string_conversion() {
        for (operation, expected) in [
            (6, "function random() { [native code] }"),
            (24, "function () { [native code] }"),
        ] {
            let program = json!([
                [2, 40, "Math"],
                [6, 41, 10, 40],
                [2, 42, "random"],
                [operation, 43, 41, 42],
                [7, 3, 43]
            ]);
            assert_eq!(
                solve(
                    &encode(program, "key"),
                    "key",
                    &profile(),
                    CancellationToken::new()
                ),
                Ok(BASE64_STANDARD.encode(expected))
            );
        }
        let program = json!([
            [2, 40, "Math"],
            [6, 41, 10, 40],
            [2, 42, "random"],
            [24, 43, 41, 42],
            [24, 44, 41, 42],
            [20, 43, 44, 4, "wrong"],
            [3, "distinct"]
        ]);
        assert_eq!(
            solve(
                &encode(program, "key"),
                "key",
                &profile(),
                CancellationToken::new()
            ),
            Ok(BASE64_STANDARD.encode("distinct"))
        );
    }

    #[test]
    fn unavailable_guest_function_source_never_becomes_a_token() {
        let program = json!([[30, 40, 41, [], []], [7, 3, 40]]);
        assert_eq!(
            solve(
                &encode(program, "key"),
                "key",
                &profile(),
                CancellationToken::new()
            ),
            Err(Error::UnsupportedCallbackSource)
        );
        let program = json!([[7, 3, 2]]);
        assert_eq!(
            solve(
                &encode(program, "key"),
                "key",
                &profile(),
                CancellationToken::new()
            ),
            Err(Error::UnsupportedInterpreterSource)
        );
    }

    #[test]
    fn json_stringify_distinguishes_top_level_and_array_undefined() {
        for (program, expected) in [
            (json!([[15, 40, 99], [7, 3, 40]]), "undefined"),
            (
                json!([[2, 40, []], [5, 40, 3], [15, 41, 40], [7, 3, 41]]),
                "[null]",
            ),
            (
                json!([
                    [2, 40, {}],
                    [2, 41, "fn"],
                    [2, 42, "Reflect"],
                    [6, 43, 10, 42],
                    [2, 44, "set"],
                    [6, 45, 43, 44],
                    [7, 45, 40, 41, 3],
                    [15, 46, 40],
                    [7, 3, 46]
                ]),
                "{}",
            ),
        ] {
            assert_eq!(
                solve(
                    &encode(program, "key"),
                    "key",
                    &profile(),
                    CancellationToken::new()
                ),
                Ok(BASE64_STANDARD.encode(expected))
            );
        }
    }

    #[test]
    fn dom_metadata_is_observed_and_canvas_pixels_are_never_faked() {
        let program = json!([
            [2, 40, "document"],
            [6, 41, 10, 40],
            [2, 42, "documentElement"],
            [6, 43, 41, 42],
            [2, 44, "getAttribute"],
            [6, 45, 43, 44],
            [2, 46, "data-build"],
            [17, 47, 45, 46],
            [7, 3, 47]
        ]);
        assert_eq!(
            solve(
                &encode(program, "key"),
                "key",
                &profile(),
                CancellationToken::new()
            ),
            Ok(BASE64_STANDARD.encode("fixture"))
        );
        let program = json!([
            [2, 40, "document"],
            [6, 41, 10, 40],
            [2, 42, "createElement"],
            [6, 43, 41, 42],
            [2, 44, "canvas"],
            [17, 45, 43, 44],
            [2, 46, "toDataURL"],
            [6, 47, 45, 46],
            [17, 48, 47],
            [3, "fake"]
        ]);
        assert_eq!(
            solve(
                &encode(program, "key"),
                "key",
                &profile(),
                CancellationToken::new()
            ),
            Err(Error::UnsupportedCanvasSerialization)
        );
        let program = json!([
            [2, 40, "document"],
            [6, 41, 10, 40],
            [2, 42, "body"],
            [6, 43, 41, 42],
            [2, 44, "getBoundingClientRect"],
            [6, 45, 43, 44],
            [17, 46, 45],
            [3, "fake"]
        ]);
        assert_eq!(
            solve(
                &encode(program, "key"),
                "key",
                &profile(),
                CancellationToken::new()
            ),
            Err(Error::UnsupportedDomLayout)
        );
    }

    #[test]
    fn absent_canvas_backends_are_null_and_required_gpu_values_stop() {
        let prefix = json!([
            [2, 40, "document"],
            [6, 41, 10, 40],
            [2, 42, "createElement"],
            [6, 43, 41, 42],
            [2, 44, "canvas"],
            [17, 45, 43, 44],
            [2, 46, "getContext"],
            [24, 47, 45, 46],
            [2, 48, "webgl"],
            [17, 49, 47, 48]
        ]);
        for (tail, expected) in [
            (
                json!([[2, 50, null], [20, 49, 50, 3, "no-context"], [4, "wrong"]]),
                Ok(BASE64_STANDARD.encode("no-context")),
            ),
            (
                json!([[2, 50, "getParameter"], [6, 51, 49, 50], [3, "fake"]]),
                Err(Error::UnsupportedCanvasContext),
            ),
        ] {
            let mut program = prefix.as_array().unwrap().clone();
            program.extend(tail.as_array().unwrap().iter().cloned());
            assert_eq!(
                solve(
                    &encode(json!(program), "key"),
                    "key",
                    &profile(),
                    CancellationToken::new()
                ),
                expected
            );
        }
    }
}
