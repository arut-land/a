use std::future::{Future, poll_fn};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};

use ext::point::ErasedHandler;
use ext::{
    BoxFuture, Called, Concurrency, Cx, DynExtension, DynFold, DynTool, Name, Parsed, PointCx,
    Refused, Registrar, Replay, Text,
};
use mlua::chunk::ChunkMode;
use mlua::{Function, HookTriggers, Lua, LuaSerdeExt, StdLib, Table, Value, VmState};
use protocol::{Audience, Card, Json, Outcome, Payload};

#[derive(Clone, Copy)]
pub struct Limits {
    pub memory: usize,
    pub instructions: u64,
    pub slice: Duration,
}

impl Limits {
    fn image(self, source: &str, tools: &[String], inherit: bool) -> mlua::Result<Json> {
        Json::encode(&(
            source,
            tools,
            self.memory,
            self.instructions,
            self.slice,
            inherit,
        ))
        .map_err(mlua::Error::external)
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory: 4 * 1024 * 1024,
            instructions: 100_000,
            slice: Duration::from_millis(25),
        }
    }
}

struct WallBudget {
    clock: Arc<dyn Fn() -> Instant + Send + Sync>,
    until: Mutex<Instant>,
    remaining: Mutex<Duration>,
}

struct Budget {
    left: AtomicU64,
    wall: Option<WallBudget>,
}

impl Budget {
    fn new(limits: Limits) -> Arc<Self> {
        Self::with_clock(limits, Instant::now)
    }

    fn with_clock(
        limits: Limits,
        clock: impl Fn() -> Instant + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            left: AtomicU64::new(limits.instructions),
            wall: Some(WallBudget {
                until: Mutex::new(clock() + limits.slice),
                remaining: Mutex::new(limits.slice),
                clock: Arc::new(clock),
            }),
        })
    }

    fn fuel(limits: Limits) -> Arc<Self> {
        Arc::new(Self {
            left: AtomicU64::new(limits.instructions),
            wall: None,
        })
    }

    fn dispatch<T>(
        self: &Arc<Self>,
        lua: &Lua,
        run: impl FnOnce() -> mlua::Result<T>,
    ) -> mlua::Result<T> {
        // scope holds the reentrant VM lock: another session cannot inherit this poll's budget.
        lua.scope(|_| {
            if lua.app_data_ref::<Arc<Budget>>().is_some() {
                self.check()?;
                return run();
            }
            let start = self.wall.as_ref().map(|wall| {
                let start = (wall.clock)();
                *locked(&wall.until) = start + *locked(&wall.remaining);
                start
            });
            lua.set_app_data(Arc::clone(self));
            let result = catch_unwind(AssertUnwindSafe(|| self.check().and_then(|_| run())));
            lua.remove_app_data::<Arc<Budget>>();
            if let Some((wall, start)) = self.wall.as_ref().zip(start) {
                let mut remaining = locked(&wall.remaining);
                *remaining = remaining.saturating_sub((wall.clock)().duration_since(start));
                if remaining.is_zero() {
                    return Err(mlua::Error::runtime("lua execution budget exhausted"));
                }
            }
            result.unwrap_or_else(|panic| resume_unwind(panic))
        })
    }

    fn check(&self) -> mlua::Result<VmState> {
        let left = self
            .left
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                left.checked_sub(1000)
            });
        if left.is_err() {
            return Err(mlua::Error::runtime("lua execution budget exhausted"));
        }
        self.check_time()
    }

    fn check_time(&self) -> mlua::Result<VmState> {
        if self
            .wall
            .as_ref()
            .is_some_and(|wall| (wall.clock)() >= *locked(&wall.until))
        {
            Err(mlua::Error::runtime("lua execution budget exhausted"))
        } else {
            Ok(VmState::Continue)
        }
    }
}

fn decode(lua: &Lua, json: &Json) -> mlua::Result<Value> {
    if json.raw().len() > 256 * 1024 {
        return Err(mlua::Error::runtime("lua JSON exceeds 256 KiB"));
    }
    let value: serde_json::Value = json.parse().map_err(mlua::Error::external)?;
    let mut work = vec![(&value, 0)];
    let mut nodes = 0;
    while let Some((value, depth)) = work.pop() {
        nodes += 1;
        if nodes > 8192 || depth > 64 {
            return Err(mlua::Error::runtime("lua JSON is too complex"));
        }
        match value {
            serde_json::Value::Array(values) => work.extend(values.iter().map(|v| (v, depth + 1))),
            serde_json::Value::Object(values) => {
                nodes += values.len();
                work.extend(values.values().map(|v| (v, depth + 1)));
            }
            _ => {}
        }
        if work.len() > 8192 {
            return Err(mlua::Error::runtime("lua JSON is too complex"));
        }
    }
    lua.to_value(&value)
}

struct Bounded(Vec<u8>);
impl std::io::Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > (256 * 1024_usize).saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("lua JSON exceeds 256 KiB"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn encode(value: &Value) -> mlua::Result<Json> {
    let mut seen = std::collections::HashSet::new();
    let mut work = vec![(value.clone(), 0)];
    let mut nodes = 0;
    while let Some((value, depth)) = work.pop() {
        nodes += 1;
        if nodes > 8192 || depth > 64 {
            return Err(mlua::Error::runtime("lua JSON is too complex"));
        }
        if let Value::Table(table) = value {
            if !seen.insert(table.to_pointer()) {
                return Err(mlua::Error::runtime("lua JSON contains a repeated table"));
            }
            for pair in table.pairs::<Value, Value>() {
                let (key, value) = pair?;
                if work.len() >= 8192 {
                    return Err(mlua::Error::runtime("lua JSON is too complex"));
                }
                work.push((key, depth + 1));
                work.push((value, depth + 1));
            }
        }
    }
    let mut output = Bounded(Vec::new());
    serde_json::to_writer(&mut output, value).map_err(mlua::Error::external)?;
    serde_json::from_slice(&output.0).map_err(mlua::Error::external)
}

#[derive(Clone)]
pub struct Mod {
    inherit: bool,
    name: Name,
    version: u32,
    image: Json,
    tools: Vec<Arc<LuaTool>>,
    hooks: Vec<(String, Arc<Hook>)>,
    texts: Vec<Text>,
    folds: Vec<Arc<LuaFold>>,
    agents: Vec<(String, LuaAgent)>,
}

#[derive(Default)]
struct Contributions {
    inherit: Option<bool>,
    tools: Vec<Table>,
    hooks: Vec<(String, Function)>,
    texts: Vec<Text>,
    folds: Vec<Table>,
    agents: Vec<Table>,
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Contributions {
    fn check(&self) -> mlua::Result<()> {
        if self.tools.len()
            + self.hooks.len()
            + self.texts.len()
            + self.folds.len()
            + self.agents.len()
            >= 128
        {
            return Err(mlua::Error::runtime("at most 128 contributions per mod"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VmMode {
    Live,
    Fold,
}

fn sandbox(limits: Limits, vm_mode: VmMode) -> mlua::Result<(Lua, Callbacks, Table)> {
    let lua = Lua::new_with(
        StdLib::TABLE | StdLib::STRING | StdLib::MATH,
        mlua::LuaOptions::default(),
    )?;
    lua.set_memory_limit(limits.memory)?;
    let callbacks = Callbacks::new(&lua)?;
    // Protected calls and user-created coroutines could swallow a hook's
    // budget error. Only the host creates resumable threads.
    for name in [
        "coroutine",
        "pcall",
        "xpcall",
        "load",
        "loadfile",
        "dofile",
        "require",
        "collectgarbage",
        "setmetatable",
        "getmetatable",
        "rawset",
        "print",
        "warn",
    ] {
        lua.globals().set(name, Value::Nil)?;
    }
    // Native pattern matching does not visit Lua's instruction hook.
    let strings: Table = lua.globals().get("string")?;
    for name in ["find", "match", "gmatch", "gsub", "dump"] {
        strings.set(name, Value::Nil)?;
    }
    for name in ["string", "table", "math"] {
        let library: Table = lua.globals().get(name)?;
        lua.globals().set(name, readonly(&lua, library)?)?;
    }
    if vm_mode == VmMode::Fold {
        lua.globals()
            .get::<Table>("math")?
            .get::<Function>("randomseed")?
            .call::<()>(0)?;
    }
    let globals = lua.create_table()?;
    for pair in lua.globals().pairs::<Value, Value>() {
        let (key, value) = pair?;
        globals.raw_set(key, value)?;
    }
    Ok((lua, callbacks, globals))
}

fn readonly(lua: &Lua, table: Table) -> mlua::Result<Table> {
    let view = lua.create_table()?;
    let meta = lua.create_table()?;
    meta.set("__index", table)?;
    meta.set(
        "__newindex",
        lua.create_function(|_, ()| -> mlua::Result<()> {
            Err(mlua::Error::runtime("read-only table"))
        })?,
    )?;
    meta.set("__metatable", false)?;
    view.set_metatable(Some(meta))?;
    Ok(view)
}

fn text_field(spec: &Table, name: &str, limit: usize) -> mlua::Result<String> {
    let value: mlua::LuaString = spec.raw_get(name)?;
    if value.as_bytes().len() > limit {
        return Err(mlua::Error::runtime(format!(
            "lua {name} exceeds {limit} bytes"
        )));
    }
    Ok(value.to_str()?.to_owned())
}

fn register(lua: &Lua, environment: &Table) -> mlua::Result<Arc<Mutex<Contributions>>> {
    let made = Arc::new(Mutex::new(Contributions::default()));
    let api = lua.create_table()?;
    let collect = Arc::downgrade(&made);
    api.set(
        "inherit",
        lua.create_function(move |_, inherit: bool| {
            let collect = collect
                .upgrade()
                .ok_or_else(|| mlua::Error::runtime("registration has ended"))?;
            let mut made = locked(&collect);
            if made.inherit.replace(inherit).is_some() {
                return Err(mlua::Error::runtime("inheritance must be declared once"));
            }
            Ok(())
        })?,
    )?;
    let collect = Arc::downgrade(&made);
    api.set(
        "agent",
        lua.create_function(move |_, spec: Table| {
            let collect = collect
                .upgrade()
                .ok_or_else(|| mlua::Error::runtime("registration has ended"))?;
            let mut made = locked(&collect);
            made.check()?;
            made.agents.push(spec);
            Ok(())
        })?,
    )?;
    let collect = Arc::downgrade(&made);
    api.set(
        "tool",
        lua.create_function(move |_, spec: Table| {
            let collect = collect
                .upgrade()
                .ok_or_else(|| mlua::Error::runtime("registration has ended"))?;
            let mut made = locked(&collect);
            made.check()?;
            made.tools.push(spec);
            Ok(())
        })?,
    )?;
    let collect = Arc::downgrade(&made);
    api.set(
        "on",
        lua.create_function(move |_, (point, run): (String, Function)| {
            let collect = collect
                .upgrade()
                .ok_or_else(|| mlua::Error::runtime("registration has ended"))?;
            let mut made = locked(&collect);
            made.check()?;
            if point.len() > 64 {
                return Err(mlua::Error::runtime("lua point name exceeds 64 bytes"));
            }
            made.hooks.push((point, run));
            Ok(())
        })?,
    )?;
    let collect = Arc::downgrade(&made);
    api.set(
        "text",
        lua.create_function(move |_, spec: Table| {
            let collect = collect
                .upgrade()
                .ok_or_else(|| mlua::Error::runtime("registration has ended"))?;
            let mut made = locked(&collect);
            made.check()?;
            let text = text_field(&spec, "text", 8192)?;
            made.texts.push(Text::system(
                &text_field(&spec, "name", 128)?,
                spec.raw_get::<Option<i32>>("order")?.unwrap_or(0),
                text,
            ));
            Ok(())
        })?,
    )?;
    let collect = Arc::downgrade(&made);
    api.set(
        "fold",
        lua.create_function(move |_, spec: Table| {
            let collect = collect
                .upgrade()
                .ok_or_else(|| mlua::Error::runtime("registration has ended"))?;
            let mut made = locked(&collect);
            made.check()?;
            let _: Function = spec.raw_get("apply")?;
            made.folds.push(spec);
            Ok(())
        })?,
    )?;
    api.set("null", lua.null())?;
    environment.set("a", readonly(lua, api)?)?;
    Ok(made)
}

impl Mod {
    pub(crate) fn restore(name: &str, image: &Json) -> Result<Self, Refused> {
        let (source, tools, memory, instructions, slice, inherit): (
            String,
            Vec<String>,
            usize,
            u64,
            Duration,
            bool,
        ) = image
            .parse()
            .map_err(|e| Refused(format!("lua {name}: {e}")))?;
        let module = Self::load(
            name,
            &source,
            &tools,
            Limits {
                memory,
                instructions,
                slice,
            },
        )?;
        if module.inherit != inherit {
            return Err(Refused(format!(
                "lua {name}: inheritance differs from generation image"
            )));
        }
        Ok(module)
    }

    /// Compiles one generation; registration is transactional.
    /// Tools, hooks and agents share mutable Lua globals across all sessions of
    /// this generation. Folds use a separate VM and fresh environments on every
    /// application: only their explicit JSON state survives, including during replay.
    pub fn load(
        name: &str,
        source: &str,
        tools: &[String],
        limits: Limits,
    ) -> Result<Self, Refused> {
        if limits.memory == 0 || limits.instructions < 1000 || limits.slice.is_zero() {
            return Err(Refused(format!(
                "lua {name}: lua limits must be positive (at least 1000 instructions)"
            )));
        }
        // Lua's parser does not run instruction hooks, so bound its input.
        // Recorded generations must prepare with the same fuel during recovery.
        if source.len() > limits.memory / 4 {
            return Err(Refused(format!(
                "lua {name}: lua preparation source exceeds budget"
            )));
        }
        Self::compile(
            name,
            source,
            tools,
            limits,
            VmMode::Live,
            &Budget::fuel(limits),
        )
        .map_err(|why| Refused(format!("lua {name}: {why}")))
    }

    fn compile(
        name: &str,
        source: &str,
        tools: &[String],
        limits: Limits,
        vm_mode: VmMode,
        budget: &Arc<Budget>,
    ) -> mlua::Result<Self> {
        let (lua, callbacks, globals) = sandbox(limits, vm_mode)?;
        let made = register(&lua, &lua.globals())?;
        let hook = Arc::clone(budget);
        let thread = lua.create_thread(
            lua.load(source)
                .set_name(name)
                .set_mode(ChunkMode::Text)
                .into_function()?,
        )?;
        thread.set_hook(
            HookTriggers::new().every_nth_instruction(1000),
            move |_, _| hook.check(),
        )?;
        thread.resume::<()>(())?;
        let mut made = std::mem::take(&mut *locked(&made));
        let name = Name::from(name);
        let grants: Arc<[String]> = tools.to_vec().into();
        let inherit = made.inherit.unwrap_or(true);
        let mut result = Self {
            inherit,
            name: Arc::clone(&name),
            version: fingerprint(source),
            image: limits.image(source, tools, inherit)?,
            tools: Vec::new(),
            hooks: Vec::new(),
            texts: std::mem::take(&mut made.texts),
            folds: Vec::new(),
            agents: Vec::new(),
        };
        if vm_mode == VmMode::Live {
            for spec in made.agents {
                result.agents.push((
                    text_field(&spec, "name", 128)?,
                    LuaAgent {
                        lua: lua.clone(),
                        run: spec.raw_get("step")?,
                        limits,
                        grants: Arc::clone(&grants),
                    },
                ));
            }
            for spec in made.tools {
                let audience = match spec.raw_get::<Option<String>>("audience")?.as_deref() {
                    None | Some("model") => Audience::Model,
                    Some("user") => Audience::User,
                    Some(_) => {
                        return Err(mlua::Error::runtime("tool audience must be model or user"));
                    }
                };
                let schema = encode(&spec.raw_get("input")?)?
                    .parse::<serde_json::Value>()
                    .map_err(mlua::Error::external)?;
                if !schema.is_object() {
                    return Err(mlua::Error::runtime("tool input must be an object schema"));
                }
                result.tools.push(Arc::new(LuaTool {
                    name: text_field(&spec, "name", 128)?,
                    description: text_field(&spec, "description", 8192)?,
                    schema,
                    audience,
                    run: spec.raw_get("run")?,
                    lua: lua.clone(),
                    owner: Arc::clone(&name),
                    grants: Arc::clone(&grants),
                    limits,
                    callbacks: callbacks.clone(),
                }));
            }
            for (point, run) in made.hooks {
                result.hooks.push((
                    point,
                    Arc::new(Hook {
                        lua: lua.clone(),
                        run,
                        limits,
                        owner: Arc::clone(&name),
                    }),
                ));
            }
        }
        if vm_mode == VmMode::Live && !made.folds.is_empty() {
            // Folding happens under the session state lock. Its VM must never
            // share a lock with callbacks that read that session's state.
            result.folds = Self::compile(&name, source, tools, limits, VmMode::Fold, budget)?.folds;
        } else if vm_mode == VmMode::Fold {
            let source: Arc<str> = source.into();
            for (slot, spec) in made.folds.into_iter().enumerate() {
                result.folds.push(Arc::new(LuaFold {
                    name: Name::from(text_field(&spec, "name", 128)?),
                    initial: encode(&spec.raw_get("initial")?)?,
                    source: Arc::clone(&source),
                    globals: globals.clone(),
                    slot,
                    lua: lua.clone(),
                    limits,
                }));
            }
        }
        Ok(result)
    }
}

fn fingerprint(source: &str) -> u32 {
    source.bytes().fold(2_166_136_261, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(16_777_619)
    })
}

impl DynExtension for Mod {
    fn inherit(&self) -> bool {
        self.inherit
    }

    fn image(&self) -> Option<Json> {
        Some(self.image.clone())
    }

    fn name(&self) -> Name {
        Arc::clone(&self.name)
    }
    fn version(&self) -> u32 {
        self.version
    }
    fn contribute(&self, r: &mut Registrar) {
        for tool in &self.tools {
            r.dyn_tool(Arc::clone(tool) as _);
        }
        for (point, handler) in &self.hooks {
            r.on_erased(point, Arc::clone(handler) as _);
        }
        for text in &self.texts {
            r.text(text.clone());
        }
        for fold in &self.folds {
            r.dyn_fold(Arc::clone(fold) as _);
        }
        for (name, agent) in &self.agents {
            let agent = agent.clone();
            r.agent(name, move || Box::new(agent.clone()));
        }
    }
}

#[derive(Clone)]
struct LuaAgent {
    lua: Lua,
    run: Function,
    limits: Limits,
    grants: Arc<[String]>,
}
impl ext::Agent for LuaAgent {
    fn step(&mut self, input: ext::Input, _: &dyn ext::LoopPoints) -> ext::Actions {
        let result = (|| {
            let input = input.to_json()?;
            let output = sync(
                &self.lua,
                &self.run,
                (
                    decode(&self.lua, &input).map_err(|e| e.to_string())?,
                    Value::Nil,
                ),
                self.limits,
            )
            .map_err(|e| e.to_string())?;
            let actions = ext::actions_from_json(&output)?;
            if actions.len() > 128 {
                return Err("lua agent returned too many actions".into());
            }
            for action in &actions {
                if let ext::Action::Call { tool, .. } = action
                    && !self.grants.contains(tool)
                {
                    return Err(format!("tool capability {tool} is not granted"));
                }
            }
            Ok(actions)
        })();
        result.unwrap_or_else(|why| {
            [ext::Action::Say(format!("lua agent failed: {why}"))]
                .into_iter()
                .collect()
        })
    }
}

fn sync(lua: &Lua, run: &Function, args: (Value, Value), limits: Limits) -> mlua::Result<Json> {
    lua.scope(|_| {
        let budget = lua.app_data_ref::<Arc<Budget>>().map_or_else(
            || ext::point::dispatch_local(0, || Budget::new(limits)),
            |budget| Arc::clone(&budget),
        );
        budget.dispatch(lua, || {
            let hook = Arc::clone(&budget);
            let thread = lua.create_thread(run.clone())?;
            thread.set_hook(
                HookTriggers::new().every_nth_instruction(1000),
                move |_, _| hook.check(),
            )?;
            encode(&thread.resume(args)?)
        })
    })
}

struct Hook {
    lua: Lua,
    run: Function,
    limits: Limits,
    owner: Name,
}
impl ErasedHandler for Hook {
    fn on(&self, cx: &PointCx<'_>, args: &Json) -> Result<Json, String> {
        let result = (|| {
            let context = self.lua.create_table()?;
            context.set("session", cx.session.to_string())?;
            let owner = Arc::clone(&self.owner);
            self.lua.scope(|scope| {
                context.set(
                    "state",
                    scope.create_function(|lua, name: String| {
                        decode(lua, &cx.state_erased(&owner, &name).unwrap_or_default())
                    })?,
                )?;
                sync(
                    &self.lua,
                    &self.run,
                    (decode(&self.lua, args)?, Value::Table(context)),
                    self.limits,
                )
            })
        })();
        result.map_err(|e| format!("lua {}: {e}", self.owner))
    }
}

struct LuaFold {
    name: Name,
    initial: Json,
    lua: Lua,
    source: Arc<str>,
    globals: Table,
    slot: usize,
    limits: Limits,
}
impl DynFold for LuaFold {
    fn name(&self) -> Name {
        Arc::clone(&self.name)
    }
    fn initial(&self) -> Json {
        self.initial.clone()
    }
    fn restore(&self, image: &Json) -> Result<Json, String> {
        decode(&self.lua, image).map_err(|e| e.to_string())?;
        Ok(image.clone())
    }
    fn apply(&self, state: &Json, change: &Json) -> Result<Json, String> {
        self.lua
            .scope(|_| {
                // Acceptance of a durable change must not depend on scheduling.
                let budget = Budget::fuel(self.limits);
                budget.dispatch(&self.lua, || {
                    let environment = self.lua.create_table()?;
                    for pair in self.globals.pairs::<Value, Value>() {
                        let (key, value) = pair?;
                        environment.raw_set(key, value)?;
                    }
                    environment.raw_set("_G", environment.clone())?;
                    // The private reducer VM's native PRNG must not carry state between applications.
                    self.globals
                        .get::<Table>("math")?
                        .get::<Function>("randomseed")?
                        .call::<()>(0)?;
                    let made = register(&self.lua, &environment)?;
                    let prepare = self
                        .lua
                        .load(&*self.source)
                        .set_mode(ChunkMode::Text)
                        .set_environment(environment)
                        .into_function()?;
                    let thread = self.lua.create_thread(prepare)?;
                    let hook = Arc::clone(&budget);
                    thread.set_hook(
                        HookTriggers::new().every_nth_instruction(1000),
                        move |_, _| hook.check(),
                    )?;
                    budget.check_time()?;
                    thread.resume::<()>(())?;
                    let contributions = std::mem::take(&mut *locked(&made));
                    drop(made);
                    let spec = contributions
                        .folds
                        .into_iter()
                        .nth(self.slot)
                        .ok_or_else(|| mlua::Error::runtime("fold registration changed"))?;
                    if text_field(&spec, "name", 128)? != *self.name {
                        return Err(mlua::Error::runtime("fold registration changed"));
                    }
                    sync(
                        &self.lua,
                        &spec.raw_get("apply")?,
                        (decode(&self.lua, state)?, decode(&self.lua, change)?),
                        self.limits,
                    )
                })
            })
            .map_err(|e| e.to_string())
    }
}

struct Quota {
    bytes: AtomicUsize,
    operations: AtomicUsize,
}
impl Quota {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            bytes: AtomicUsize::new(1024 * 1024),
            operations: AtomicUsize::new(128),
        })
    }
    fn take(&self, bytes: usize) -> mlua::Result<()> {
        // The VM allocator cannot account for data retained in the host's log.
        let operations = self
            .operations
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                left.checked_sub(1)
            });
        let bytes = self
            .bytes
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                left.checked_sub(bytes)
            });
        if operations.is_err() || bytes.is_err() {
            return Err(mlua::Error::runtime("lua outbound quota exhausted"));
        }
        Ok(())
    }
}

#[derive(Clone)]
struct Call {
    cx: std::sync::Weak<Cx>,
    owner: Name,
    grants: Arc<[String]>,
    quota: Arc<Quota>,
    permit: Arc<Permit>,
}
struct ActiveCall {
    calls: Arc<Mutex<std::collections::BTreeMap<u64, Call>>>,
    id: u64,
}
impl Drop for ActiveCall {
    fn drop(&mut self) {
        locked(&self.calls).remove(&self.id);
    }
}

#[derive(Clone)]
struct Callbacks {
    calls: Arc<Mutex<std::collections::BTreeMap<u64, Call>>>,
    next: Arc<AtomicU64>,
    admission: Arc<Mutex<Admission>>,
    update: Function,
    publish: Function,
    published: Function,
    invoke: Function,
}
impl Callbacks {
    fn new(lua: &Lua) -> mlua::Result<Self> {
        let calls = Arc::new(Mutex::new(std::collections::BTreeMap::<u64, Call>::new()));
        let updates = Arc::clone(&calls);
        let update =
            lua.create_async_function(move |_lua, (id, name, value): (u64, String, Value)| {
                let data = locked(&updates).get(&id).cloned();
                async move {
                    let data = data.ok_or_else(|| mlua::Error::runtime("call context expired"))?;
                    let cx = data
                        .cx
                        .upgrade()
                        .ok_or_else(|| mlua::Error::runtime("call context expired"))?;
                    if cx.state_erased(&data.owner, &name).is_none() {
                        return Err(mlua::Error::runtime("no fold of that name"));
                    }
                    let change = encode(&value)?;
                    data.quota.take(change.raw().len())?;
                    cx.update_erased(&data.owner, &name, change)
                        .await
                        .map_err(mlua::Error::external)?;
                    Ok(())
                }
            })?;
        let publications = Arc::clone(&calls);
        let publish =
            lua.create_async_function(move |_, (id, key, text): (u64, Value, String)| {
                let data = locked(&publications).get(&id).cloned();
                async move {
                    let data = data.ok_or_else(|| mlua::Error::runtime("call context expired"))?;
                    let cx = data
                        .cx
                        .upgrade()
                        .ok_or_else(|| mlua::Error::runtime("call context expired"))?;
                    let key = encode(&key)?;
                    data.quota.take(key.raw().len() + text.len())?;
                    cx.publish_erased(&format!("lua:{}", data.owner), key, text)
                        .await
                        .map_err(mlua::Error::external)?;
                    Ok(())
                }
            })?;
        let publications = Arc::clone(&calls);
        let published = lua.create_function(move |_, (id, key): (u64, Value)| {
            let data = locked(&publications)
                .get(&id)
                .cloned()
                .ok_or_else(|| mlua::Error::runtime("call context expired"))?;
            let cx = data
                .cx
                .upgrade()
                .ok_or_else(|| mlua::Error::runtime("call context expired"))?;
            let key = encode(&key)?;
            data.quota.take(key.raw().len())?;
            Ok(cx.published_erased(&format!("lua:{}", data.owner), &key))
        })?;
        let invokes = Arc::clone(&calls);
        let invoke = lua.create_async_function(
            move |lua, (id, name, tool, value): (u64, String, String, Value)| {
                let data = locked(&invokes).get(&id).cloned();
                async move {
                    let data = data.ok_or_else(|| mlua::Error::runtime("call context expired"))?;
                    if !data.grants.contains(&tool) {
                        return Err(mlua::Error::runtime(format!(
                            "tool capability {tool} is not granted"
                        )));
                    }
                    let cx = data
                        .cx
                        .upgrade()
                        .ok_or_else(|| mlua::Error::runtime("call context expired"))?;
                    let input = encode(&value)?;
                    data.quota.take(input.raw().len())?;
                    data.permit.release();
                    let called = cx.invoke(&name, &tool, &input).await;
                    data.permit.acquire().await?;
                    match called.outcome {
                        Outcome::Done {
                            output: Payload::Inline(output),
                        } => decode(&lua, &output),
                        Outcome::Done {
                            output: Payload::Blob(_),
                        } => Err(mlua::Error::runtime("delegated tool output held as a blob")),
                        Outcome::Failed { error } => Err(mlua::Error::runtime(error)),
                        Outcome::Interrupted { .. } => {
                            Err(mlua::Error::runtime("delegated tool interrupted"))
                        }
                    }
                }
            },
        )?;
        Ok(Self {
            calls,
            next: Arc::new(AtomicU64::new(0)),
            admission: Arc::default(),
            update,
            publish,
            published,
            invoke,
        })
    }
}

#[derive(Default)]
struct Admission {
    active: usize,
    waiters: std::collections::BTreeMap<u64, Waker>,
}

struct Permit {
    admission: Arc<Mutex<Admission>>,
    id: u64,
    active: AtomicBool,
}
impl Drop for Permit {
    fn drop(&mut self) {
        self.release();
    }
}
impl Permit {
    async fn acquire(&self) -> mlua::Result<()> {
        poll_fn(|cx| {
            let mut admission = locked(&self.admission);
            if self.active.load(Ordering::Relaxed) {
                return Poll::Ready(Ok(()));
            }
            if admission.active < 16 {
                admission.waiters.remove(&self.id);
                admission.active += 1;
                self.active.store(true, Ordering::Relaxed);
                Poll::Ready(Ok(()))
            } else if admission.waiters.len() >= 64 && !admission.waiters.contains_key(&self.id) {
                Poll::Ready(Err(mlua::Error::runtime("lua execution admission full")))
            } else {
                admission.waiters.insert(self.id, cx.waker().clone());
                Poll::Pending
            }
        })
        .await
    }

    fn release(&self) {
        let mut admission = locked(&self.admission);
        admission.waiters.remove(&self.id);
        if !self.active.swap(false, Ordering::Relaxed) {
            return;
        }
        admission.active -= 1;
        let waiters: Vec<_> = admission.waiters.values().cloned().collect();
        drop(admission);
        for waker in waiters {
            waker.wake();
        }
    }
}

struct LuaTool {
    name: String,
    description: String,
    schema: serde_json::Value,
    audience: Audience,
    lua: Lua,
    run: Function,
    owner: Name,
    grants: Arc<[String]>,
    limits: Limits,
    callbacks: Callbacks,
}

impl LuaTool {
    async fn execute(&self, input: &Json, cx: &Cx) -> mlua::Result<Json> {
        let id = self.callbacks.next.fetch_add(1, Ordering::Relaxed);
        let permit = Arc::new(Permit {
            admission: Arc::clone(&self.callbacks.admission),
            id,
            active: AtomicBool::new(false),
        });
        permit.acquire().await?;
        let context = self.lua.create_table()?;
        context.set("session", cx.session.to_string())?;
        let call = Arc::new(cx.clone());
        let read = Arc::downgrade(&call);
        let owner = Arc::clone(&self.owner);
        context.set(
            "state",
            self.lua.create_function(move |lua, name: String| {
                decode(
                    lua,
                    &read
                        .upgrade()
                        .ok_or_else(|| mlua::Error::runtime("call context expired"))?
                        .state_erased(&owner, &name)
                        .unwrap_or_default(),
                )
            })?,
        )?;
        let quota = Quota::new();
        let active = ActiveCall {
            calls: Arc::clone(&self.callbacks.calls),
            id,
        };
        locked(&self.callbacks.calls).insert(
            id,
            Call {
                cx: Arc::downgrade(&call),
                owner: Arc::clone(&self.owner),
                grants: Arc::clone(&self.grants),
                quota: Arc::clone(&quota),
                permit: Arc::clone(&permit),
            },
        );
        context.set("update", self.callbacks.update.bind(id)?)?;
        context.set("publish", self.callbacks.publish.bind(id)?)?;
        context.set("published", self.callbacks.published.bind(id)?)?;
        context.set("tool", self.callbacks.invoke.bind(id)?)?;
        let progress = Arc::downgrade(&call);
        context.set(
            "progress",
            self.lua.create_function(move |_, text: String| {
                quota.take(text.len())?;
                progress
                    .upgrade()
                    .ok_or_else(|| mlua::Error::runtime("call context expired"))?
                    .progress(&text);
                Ok(())
            })?,
        )?;
        let budget = cx.invocation().local(0, || Budget::new(self.limits));
        let hook = Arc::clone(&budget);
        let thread = self.lua.create_thread(self.run.clone())?;
        thread.set_hook(
            HookTriggers::new().every_nth_instruction(1000),
            move |_, _| hook.check(),
        )?;
        let mut work =
            pin!(thread.into_async::<Value>((decode(&self.lua, input)?, Value::Table(context)))?);
        let value = poll_fn(|poll| {
            match cx
                .invocation()
                .scope(|| budget.dispatch(&self.lua, || Ok(work.as_mut().poll(poll))))
            {
                Ok(poll) => poll,
                Err(error) => Poll::Ready(Err(error)),
            }
        })
        .await?;
        drop(active);
        drop(call);
        encode(&value)
    }
}

impl DynTool for LuaTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn audience(&self) -> Audience {
        self.audience
    }
    fn replay(&self) -> Replay {
        Replay::Unsafe
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
    }
    fn spec(&self) -> llm::Tool {
        llm::Tool {
            name: self.name.as_str().into(),
            description: self.description.clone(),
            schema: self.schema.clone(),
            strict: None,
        }
    }
    fn parse(&self, input: &Json) -> Result<Parsed, String> {
        Ok(Parsed::new(input.clone()))
    }
    fn call<'a>(&'a self, input: Parsed, cx: &'a Cx) -> BoxFuture<'a, Called> {
        Box::pin(async move {
            let Some(input) = input.get::<Json>() else {
                return Called::failed("lua tool input was not JSON".into());
            };
            match self.execute(input, cx).await {
                Ok(output) => Called {
                    outcome: Outcome::Done {
                        output: output.into(),
                    },
                    card: None,
                    held: Vec::new(),
                },
                Err(error) => Called::failed(format!("lua {}: {error}", self.owner)),
            }
        })
    }
    fn to_model(&self, output: &Json) -> llm::Output {
        llm::Output::Json(output.clone())
    }
    fn present(&self, _: &Parsed) -> Option<Card> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ext::host::Registry;
    use ext::testing::Rig;
    use ext::{Extension, Registrar, Tool};
    use serde_json::json;

    fn registry(source: &str, grants: &[String]) -> Registry {
        let module = Mod::load("test", source, grants, Limits::default()).unwrap();
        Registry::default()
            .with_dyn(ext::Tier::User, &module)
            .unwrap()
    }

    fn tool(body: &str) -> String {
        format!(
            "a.tool{{name='test',description='test',input={{type='object'}},run=function(input,cx) {body} end}}"
        )
    }

    #[test]
    fn callback_chains_share_one_dispatch_budget() {
        let limits = Limits {
            instructions: 10_000,
            slice: Duration::from_secs(1),
            ..Limits::default()
        };
        let source = "for i=1,8 do a.on('input',function(input) if input=='slow' then local n=0; for j=1,1000 do n=n+j end end; return a.null end) end";
        let module = Mod::load("chain", source, &[], limits).unwrap();
        let rig = Rig::new(
            "lua-hook-chain",
            Registry::default()
                .with_dyn(ext::Tier::User, &module)
                .unwrap(),
        );
        let cx = rig.cx();
        assert!(
            rig.registry
                .hooks()
                .input(&cx.point(), "slow".into())
                .unwrap_err()
                .contains("budget")
        );
        assert_eq!(
            rig.registry
                .hooks()
                .input(&cx.point(), "fast".into())
                .unwrap()
                .value,
            "fast"
        );
        let (lua, _, _) = sandbox(limits, VmMode::Live).unwrap();
        let inner = lua
            .load("return function() local n=0; for i=1,1000 do n=n+i end; return n end")
            .eval::<Function>()
            .unwrap();
        lua.globals()
            .set(
                "chain",
                lua.create_function(move |lua, ()| {
                    for _ in 0..20 {
                        sync(lua, &inner, (Value::Nil, Value::Nil), limits)?;
                    }
                    Ok(())
                })
                .unwrap(),
            )
            .unwrap();
        let run = lua
            .load("return function() chain(); return true end")
            .eval::<Function>()
            .unwrap();
        assert!(
            sync(&lua, &run, (Value::Nil, Value::Nil), limits)
                .unwrap_err()
                .to_string()
                .contains("budget")
        );
        assert!(lua.app_data_ref::<Arc<Budget>>().is_none());
        let run = lua
            .load("return function() return true end")
            .eval::<Function>()
            .unwrap();
        assert!(
            sync(&lua, &run, (Value::Nil, Value::Nil), limits)
                .unwrap()
                .parse::<bool>()
                .unwrap()
        );
    }

    #[test]
    fn cancelled_lua_waiters_release_admission() {
        let module = Mod::load(
            "cancel",
            &tool("return cx.tool('wait','wait',{args='park'})"),
            &["wait".into()],
            Limits::default(),
        )
        .unwrap();
        let rig = Rig::new(
            "lua-admission",
            Registry::default().with(&Service(false)).unwrap(),
        );
        let cx = rig.cx();
        let input = Json::default();
        let admission = &module.tools[0].callbacks.admission;
        let mut held: Vec<_> = (0..16)
            .map(|id| {
                Arc::new(Permit {
                    admission: Arc::clone(admission),
                    id,
                    active: AtomicBool::new(false),
                })
            })
            .collect();
        for permit in &held {
            rt::block_on(permit.acquire()).unwrap();
        }
        module.tools[0].callbacks.next.store(16, Ordering::Relaxed);
        let mut context = std::task::Context::from_waker(Waker::noop());
        let mut calls: Vec<_> = (0..64)
            .map(|_| Box::pin(module.tools[0].execute(&input, &cx)))
            .collect();
        for call in &mut calls {
            assert!(call.as_mut().poll(&mut context).is_pending());
        }
        assert_eq!(locked(admission).active, 16);
        assert_eq!(locked(admission).waiters.len(), 64);
        assert!(
            rt::block_on(module.tools[0].execute(&input, &cx))
                .unwrap_err()
                .to_string()
                .contains("admission full")
        );
        calls.pop();
        assert_eq!(locked(admission).waiters.len(), 63);
        let mut replacement = Box::pin(module.tools[0].execute(&input, &cx));
        assert!(replacement.as_mut().poll(&mut context).is_pending());
        drop(held.pop());
        assert!(replacement.as_mut().poll(&mut context).is_pending());
        assert_eq!(locked(admission).active, 15);
        drop(replacement);
        drop(calls);
        drop(held);
        assert_eq!(locked(admission).active, 0);
        assert!(locked(admission).waiters.is_empty());
        assert!(locked(&module.tools[0].callbacks.calls).is_empty());
        let mut fresh = Box::pin(module.tools[0].execute(&input, &cx));
        assert!(fresh.as_mut().poll(&mut context).is_pending());
        assert_eq!(locked(admission).active, 0);
        drop(fresh);
        assert_eq!(locked(admission).active, 0);
    }

    #[test]
    fn admitted_owners_leave_capacity_for_their_delegated_calls() {
        #[derive(Clone)]
        struct Release(Arc<AtomicBool>);
        impl Tool for Release {
            type In = ext::Args;
            type Out = bool;
            const NAME: &'static str = "release";
            const DESCRIPTION: &'static str = "wait for release";
            async fn call(&self, _: &ext::Args, _: &Cx) -> Result<bool, String> {
                poll_fn(|_| {
                    if self.0.load(Ordering::SeqCst) {
                        Poll::Ready(Ok(true))
                    } else {
                        Poll::Pending
                    }
                })
                .await
            }
        }
        impl Extension for Release {
            const NAME: &'static str = "release";
            const VERSION: u32 = 1;
            fn contribute(&self, r: &mut Registrar) {
                r.tool(self.clone());
            }
        }
        let release = Release(Arc::default());
        let module = Mod::load(
            "test",
            &tool("if input.leaf then return true end; cx.tool('release','release',{}); return cx.tool('leaf','test',{leaf=true})"),
            &["release".into(), "test".into()],
            Limits::default(),
        )
        .unwrap();
        let registry = Registry::default()
            .with(&release)
            .unwrap()
            .with_dyn(ext::Tier::User, &module)
            .unwrap();
        let rigs: Vec<_> = (0..16)
            .map(|i| Rig::new(&format!("lua-nested-admission-{i}"), registry.clone()))
            .collect();
        let contexts: Vec<_> = rigs.iter().map(Rig::cx).collect();
        let input = Json::encode(&json!({})).unwrap();
        let mut calls: Vec<_> = contexts
            .iter()
            .map(|cx| Box::pin(module.tools[0].execute(&input, cx)))
            .collect();
        let mut context = std::task::Context::from_waker(Waker::noop());
        for call in &mut calls {
            assert!(call.as_mut().poll(&mut context).is_pending());
        }
        release.0.store(true, Ordering::SeqCst);
        let completed = calls
            .iter_mut()
            .map(|call| match call.as_mut().poll(&mut context) {
                Poll::Ready(result) => {
                    assert!(result.unwrap().parse::<bool>().unwrap());
                    true
                }
                Poll::Pending => false,
            })
            .filter(|ready| *ready)
            .count();
        let admission = locked(&module.tools[0].callbacks.admission);
        assert!(
            completed == 16,
            "all owners await children: {} active owners, {} queued children",
            admission.active,
            admission.waiters.len()
        );
    }

    #[test]
    fn durable_reducers_ignore_deadlines_but_replay_fuel_rejections() {
        let source = "a.fold{name='count',initial=0,apply=function(state,change) if change<0 then while true do end end; return state+change end}; a.tool{name='test',description='test',input={type='object'},run=function(_,cx) cx.update('count',1); return cx.state('count') end}";
        let mut module = Mod::load("test", source, &[], Limits::default()).unwrap();
        Arc::get_mut(&mut module.folds[0]).unwrap().limits.slice = Duration::ZERO;
        let rig = Rig::new(
            "lua-fuel-replay",
            Registry::default()
                .with_dyn(ext::Tier::User, &module)
                .unwrap(),
        );
        assert_eq!(rig.done("test", &json!({})), 1);
        let rejected = rt::block_on(rig.cx().update_erased(
            "test",
            "count",
            Json::encode(&-1).unwrap(),
        ));
        assert!(
            matches!(&rejected, Err(ext::MutationError::Rejected(error)) if error.contains("budget"))
        );
        let mut replay = rig.registry.states();
        let log = rig.log.lock().unwrap();
        replay.replay(log.iter());
        assert_eq!(
            replay
                .erased("test", "count")
                .unwrap()
                .parse::<u64>()
                .unwrap(),
            1
        );
        assert_eq!(replay.mutation_result(log.last().unwrap()), rejected);
    }

    #[test]
    fn non_durable_native_work_spends_the_injected_clock_budget() {
        let start = Instant::now();
        let elapsed = Arc::new(AtomicU64::new(0));
        let budget = Budget::with_clock(Limits::default(), {
            let elapsed = Arc::clone(&elapsed);
            move || start + Duration::from_millis(elapsed.load(Ordering::Relaxed))
        });
        let lua = Lua::new();
        assert!(
            budget
                .dispatch(&lua, || {
                    elapsed.store(100, Ordering::Relaxed);
                    Ok(())
                })
                .unwrap_err()
                .to_string()
                .contains("budget")
        );
        assert!(lua.app_data_ref::<Arc<Budget>>().is_none());
    }

    #[test]
    fn fold_replay_has_no_tool_vm_state() {
        let source = "local transient=0; a.tool{name='test',description='test',input={type='object'},run=function(_,cx) transient=transient+1; cx.update('count',1); return transient end}; a.fold{name='count',initial=0,apply=function(state,change) return state+change+transient end}";
        let module = Mod::load("test", source, &[], Limits::default()).unwrap();
        let registry = Registry::default()
            .with_dyn(ext::Tier::User, &module)
            .unwrap();
        let rig = Rig::new("lua-isolation", registry.clone());
        assert_eq!(rig.done("test", &json!({})), 1);
        let other = Rig::new("lua-shared-generation", registry);
        assert_eq!(other.done("test", &json!({})), 2);
        rig.restart();
        assert_eq!(rig.done("test", &json!({})), 3);
        let mut replay = rig.registry.states();
        replay.replay(rig.log.lock().unwrap().iter());
        assert_eq!(
            replay
                .erased("test", "count")
                .unwrap()
                .parse::<u64>()
                .unwrap(),
            2
        );
    }

    #[test]
    fn another_sessions_reducer_calls_cannot_change_replay() {
        let source = "local transient=0; a.fold{name='count',initial=0,apply=function(state,change) transient=transient+1; return state+change+transient end}; a.tool{name='test',description='test',input={type='object'},run=function(_,cx) cx.update('count',1); return cx.state('count') end}";
        let module = Mod::load("test", source, &[], Limits::default()).unwrap();
        let registry = Registry::default()
            .with_dyn(ext::Tier::User, &module)
            .unwrap();
        let first = Rig::new("lua-reducer-first", registry.clone());
        let second = Rig::new("lua-reducer-second", registry);
        assert_eq!(first.done("test", &json!({})), 2);
        assert_eq!(second.done("test", &json!({})), 2);
        assert_eq!(first.done("test", &json!({})), 4);

        let fresh = Mod::load("test", source, &[], Limits::default()).unwrap();
        let mut replay = Registry::default()
            .with_dyn(ext::Tier::User, &fresh)
            .unwrap()
            .states();
        replay.replay(first.log.lock().unwrap().iter());
        assert_eq!(
            replay
                .erased("test", "count")
                .unwrap()
                .parse::<u64>()
                .unwrap(),
            4,
            "replay must reproduce the first session without the second session's log"
        );
    }

    #[test]
    fn reducers_reset_globals_and_native_random_state() {
        let source = "a.fold{name='count',initial={count=0},apply=function(state,change) _G.counter=(_G.counter or 0)+1; return {count=state.count+change+_G.counter,random=math.random()} end}; a.tool{name='test',description='test',input={type='object'},run=function(_,cx) cx.update('count',1); return cx.state('count') end}";
        let registry = registry(source, &[]);
        let first = Rig::new("lua-fresh-globals", registry.clone());
        let second = Rig::new("lua-other-globals", registry);
        let initial = first.done("test", &json!({}));
        assert_eq!(second.done("test", &json!({})), initial);
        let live = first.done("test", &json!({}));
        assert_eq!(live["count"], 4);
        assert_eq!(live["random"], initial["random"]);
        let mut replay = tests::registry(source, &[]).states();
        replay.replay(first.log.lock().unwrap().iter());
        assert_eq!(
            replay
                .erased("test", "count")
                .unwrap()
                .parse::<serde_json::Value>()
                .unwrap(),
            live
        );
    }

    #[test]
    fn recorded_generation_preparation_depends_on_fuel_not_elapsed_time() {
        let limits = Limits {
            slice: Duration::from_nanos(1),
            ..Limits::default()
        };
        let source =
            "a.fold{name='count',initial=0,apply=function(state,change) return state+change end}";
        let module = Mod::load("test", source, &[], limits).unwrap();
        let restored = Mod::restore("test", &module.image).unwrap();
        assert_eq!(restored.image, module.image);
        for module in [&module, &restored] {
            assert_eq!(
                module.folds[0]
                    .apply(&Json::encode(&1).unwrap(), &Json::encode(&2).unwrap())
                    .unwrap()
                    .parse::<u64>()
                    .unwrap(),
                3
            );
        }
    }

    #[test]
    fn preparation_rejects_oversized_source_and_exhausted_instructions() {
        let limits = Limits::default();
        let source = " ".repeat(limits.memory / 4 + 1);
        assert!(
            Mod::load("large", &source, &[], limits)
                .err()
                .unwrap()
                .0
                .contains("preparation source")
        );
        let limits = Limits {
            instructions: 6_000,
            slice: Duration::from_secs(1),
            ..Limits::default()
        };
        let source = "local n=0; for i=1,2000 do n=n+i end; a.fold{name='count',initial=0,apply=function(s,c) return s+c end}";
        assert!(
            Mod::load("aggregate", source, &[], limits)
                .err()
                .unwrap()
                .0
                .contains("budget")
        );
    }

    #[test]
    fn lua_inheritance_is_a_restorable_whole_generation_decision() {
        let source = "a.inherit(false); a.text{name='root',text='root only'}; a.agent{name='root',step=function() return {} end}";
        let module = Mod::load("root", source, &[], Limits::default()).unwrap();
        let restored = Mod::restore("root", &module.image).unwrap();
        for module in [&module, &restored] {
            assert!(!module.inherit());
            let registry = Registry::default()
                .with_dyn(ext::Tier::User, module)
                .unwrap();
            assert_eq!(registry.installed().len(), 1);
            let inherited = registry.inherited().unwrap();
            assert_eq!(inherited.extensions().count(), 0);
            assert_eq!(inherited.agents().collect::<Vec<_>>(), [ext::LOOP]);
            assert_eq!(inherited.system_prompt(), "");
        }
        let default = Mod::load("default", "", &[], Limits::default()).unwrap();
        assert!(Mod::restore("default", &default.image).unwrap().inherit());
        let mut image: serde_json::Value = module.image.parse().unwrap();
        image[5] = true.into();
        assert!(Mod::restore("root", &Json::encode(&image).unwrap()).is_err());
    }

    #[test]
    fn lua_publications_replay_as_one_context_entry() {
        let source = tool(
            "if not cx.published('guide') then cx.publish('guide','instructions') end; return cx.published('guide')",
        );
        let rig = Rig::new("lua-publication", registry(&source, &[]));
        assert_eq!(rig.done("test", &json!({})), true);
        rig.restart();
        assert_eq!(rig.done("test", &json!({})), true);
        let log = rig.log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert!(matches!(
            &log[0].body,
            protocol::Body::Context {
                publication: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn lua_agents_return_sdk_actions_and_fail_within_their_budget() {
        let registry = registry(
            "a.agent{name='scripted',step=function(input) if input.user then return {{call={tool='test',input={text=input.user.text}}}} end; return {} end}",
            &["test".into()],
        );
        let mut agent = registry.agent(Some("scripted"));
        let actions = agent.step(ext::Input::User { text: "hi".into() }, &ext::Defaults);
        assert!(
            matches!(&actions[..], [ext::Action::Call { tool, input, .. }] if tool=="test" && input.parse::<serde_json::Value>().unwrap()==json!({"text":"hi"}))
        );
        let registry = tests::registry(
            "a.agent{name='scripted',step=function() while true do end end}",
            &[],
        );
        let mut agent = registry.agent(Some("scripted"));
        assert!(
            matches!(&agent.step(ext::Input::User { text: "hi".into() }, &ext::Defaults)[..], [ext::Action::Say(text)] if text.contains("budget"))
        );
    }

    #[test]
    fn agent_proposals_cannot_bypass_tool_capabilities() {
        let registry = registry(
            "a.agent{name='scripted',step=function() return {{call={tool='bash',input={command='pwd'}}}} end}",
            &[],
        );
        let mut agent = registry.agent(Some("scripted"));
        assert!(
            matches!(&agent.step(ext::Input::User { text: "hi".into() }, &ext::Defaults)[..], [ext::Action::Say(text)] if text.contains("not granted"))
        );
    }

    #[test]
    fn the_documented_example_registers_and_runs() {
        let rig = Rig::new(
            "lua-example",
            registry(include_str!("../../../docs/examples/counter.lua"), &[]),
        );
        assert_eq!(rig.done("count", &json!({"args":""})), json!({"count":1}));
        assert!(rig.registry.system_prompt().contains("/count"));
    }

    #[test]
    fn json_values_and_null_cross_the_boundary_without_losing_their_shape() {
        let rig = Rig::new("lua-json", registry(&tool("return input"), &[]));
        let input = json!({"empty": [], "null": null, "object": {}, "text": "a\n☃", "bool": false});
        assert_eq!(rig.done("test", &input), input);
    }

    #[test]
    fn durable_folds_are_per_session_and_replay_from_the_log() {
        let source = tool("cx.update('count',input.n); return cx.state('count')")
            + "\na.fold{name='count',initial=0,apply=function(state,change) return state+change end}";
        let rig = Rig::new("lua-fold", registry(&source, &[]));
        assert_eq!(rig.done("test", &json!({"n":2})), 2);
        rig.restart();
        assert_eq!(rig.done("test", &json!({"n":3})), 5);
        let mut restored = rig.registry.states();
        let mut states = rig.registry.states();
        states.replay(rig.log.lock().unwrap().iter());
        restored.restore(&states.image().unwrap()).unwrap();
        assert_eq!(
            restored
                .erased("test", "count")
                .unwrap()
                .parse::<u64>()
                .unwrap(),
            5
        );
        let other = Rig::new("lua-other-session", registry(&source, &[]));
        assert_eq!(other.done("test", &json!({"n":1})), 1);
    }

    #[test]
    fn outbound_data_cannot_escape_the_per_call_memory_budget() {
        let source = tool(
            "if input.spam then while true do cx.progress(string.rep('x',16384)) end end; if input.update then while true do cx.update('count',1) end end; return 7",
        ) + "a.fold{name='count',initial=0,apply=function(state,change) return state+change end}";
        let module = Mod::load(
            "test",
            &source,
            &[],
            Limits {
                slice: Duration::from_secs(1),
                ..Limits::default()
            },
        )
        .unwrap();
        let rig = Rig::new(
            "lua-outbound",
            Registry::default()
                .with_dyn(ext::Tier::User, &module)
                .unwrap(),
        );
        assert!(
            matches!(rig.call("test", &json!({"spam":true})), Outcome::Failed { error } if error.contains("quota"))
        );
        assert!(rig.progress.lock().unwrap().len() <= 1024 * 1024);
        assert!(
            matches!(rig.call("test", &json!({"update":true})), Outcome::Failed { error } if error.contains("quota"))
        );
        assert_eq!(rig.log.lock().unwrap().len(), 128);
        assert_eq!(rig.done("test", &json!({})), 7);
    }

    #[test]
    fn loops_and_allocation_failures_leave_other_calls_usable() {
        let source = tool(
            "if input.loop then while true do end end; if input.allocate then return string.rep('x', 8000000) end; return 7",
        );
        let rig = Rig::new("lua-budgets", registry(&source, &[]));
        for input in [json!({"loop":true}), json!({"allocate":true})] {
            assert!(matches!(rig.call("test", &input), Outcome::Failed { .. }));
            assert_eq!(rig.done("test", &json!({})), 7);
        }
        assert!(Mod::load("loop", "while true do end", &[], Limits::default()).is_err());
    }

    #[test]
    fn handler_failures_use_each_points_error_rule() {
        let rig = Rig::new(
            "lua-point-errors",
            registry(
                "a.on('input',function() error('broken') end); a.on('on_stop',function() while true do end end)",
                &[],
            ),
        );
        let cx = rig.cx();
        assert!(
            rig.registry
                .hooks()
                .input(&cx.point(), "hi".into())
                .unwrap_err()
                .contains("broken")
        );
        assert!(rig.registry.hooks().on_stop(&cx.point()).is_none());
    }

    #[test]
    fn the_sandbox_has_no_ambient_effects_or_catchable_budget_errors() {
        let rig = Rig::new(
            "lua-sandbox",
            registry(
                &tool(
                    "assert(os==nil and io==nil and debug==nil and package==nil and require==nil and load==nil and dofile==nil and pcall==nil and xpcall==nil and coroutine==nil and setmetatable==nil and getmetatable==nil); return true",
                ),
                &[],
            ),
        );
        assert_eq!(rig.done("test", &json!({})), true);
    }

    #[test]
    fn inbound_json_has_the_same_depth_and_node_limits_as_output() {
        let lua = Lua::new();
        let source = format!(
            "a.fold{{name='count', initial=0, apply=function(s,c) return s end}}; {}",
            tool("return true")
        );
        let rig = Rig::new("lua-inbound", registry(&source, &[]));
        for text in [
            format!("{}0{}", "[".repeat(80), "]".repeat(80)),
            format!("[{}]", vec!["0"; 9000].join(",")),
        ] {
            let json: Json = serde_json::from_str(&text).unwrap();
            assert!(decode(&lua, &json).is_err());
            assert!(matches!(
                rig.call("test", &json.parse().unwrap()),
                Outcome::Failed { .. }
            ));
            assert!(
                rig.registry
                    .states()
                    .restore(&ext::Checkpoint {
                        folds: vec![ext::Image {
                            ext: "test".into(),
                            state: "count".into(),
                            image: json,
                            rejected: std::collections::BTreeMap::new(),
                        }],
                        ..ext::Checkpoint::default()
                    })
                    .is_err()
            );
        }
    }

    struct ComplexResult;
    impl Tool for ComplexResult {
        type In = ext::Args;
        type Out = serde_json::Value;
        const NAME: &'static str = "complex";
        const DESCRIPTION: &'static str = "Return a complex value.";
        fn call(&self, args: &ext::Args, _: &Cx) -> Result<Self::Out, String> {
            let text = if args.args == "depth" {
                format!("{}0{}", "[".repeat(80), "]".repeat(80))
            } else {
                format!("[{}]", vec!["0"; 9000].join(","))
            };
            serde_json::from_str(&text).map_err(|e| e.to_string())
        }
    }
    impl Extension for ComplexResult {
        const NAME: &'static str = "complex";
        const VERSION: u32 = 1;
        fn contribute(&self, r: &mut Registrar) {
            r.tool(Self);
        }
    }

    #[test]
    fn delegated_results_are_bounded_before_lua_can_consume_them() {
        let rig = Rig::new(
            "lua-delegated-limits",
            registry(
                &tool("cx.tool('complex','complex',input); return true"),
                &["complex".into()],
            )
            .with(&ComplexResult)
            .unwrap(),
        );
        for args in ["depth", "nodes"] {
            assert!(matches!(
                rig.call("test", &json!({"args":args})),
                Outcome::Failed { .. }
            ));
        }
    }

    #[test]
    fn shared_sandbox_tables_cannot_be_changed_or_dump_bytecode() {
        assert!(Mod::load("dump", "assert(string.dump == nil)", &[], Limits::default()).is_ok());
        for source in [
            "string.len = nil",
            "table.insert = nil",
            "math.abs = nil",
            "a.null = nil",
            "rawset(a, 'tool', nil)",
            "table.insert(a, true)",
        ] {
            assert!(
                Mod::load("readonly", source, &[], Limits::default()).is_err(),
                "{source}"
            );
        }
    }

    #[test]
    fn hostile_registration_and_json_cannot_execute_unbudgeted_code() {
        assert!(
            Mod::load(
                "metatable",
                "a.tool(setmetatable({}, {__index=function() while true do end end}))",
                &[],
                Limits::default()
            )
            .is_err()
        );
        let rig = Rig::new(
            "lua-json-limits",
            registry(
                &tool(
                    "local t={}; if input.alias then return {t,t} end; for i=1,100 do t={t} end; return t",
                ),
                &[],
            ),
        );
        for input in [json!({}), json!({"alias":true})] {
            assert!(matches!(rig.call("test", &input), Outcome::Failed { .. }));
        }
    }

    #[test]
    fn saved_call_contexts_expire_when_their_call_finishes() {
        let source =
            tool("if input.save then saved=cx.state; return true end; return saved('count')");
        let rig = Rig::new("lua-context", registry(&source, &[]));
        assert_eq!(rig.done("test", &json!({"save":true})), true);
        assert!(
            matches!(rig.call("test", &json!({})), Outcome::Failed { error } if error.contains("expired"))
        );
    }

    #[test]
    fn cancelling_a_suspended_call_releases_its_host_handle() {
        let module = Mod::load(
            "cancel",
            &tool("return cx.tool('wait','wait',{})"),
            &["wait".into()],
            Limits::default(),
        )
        .unwrap();
        let rig = Rig::new(
            "lua-cancel-handle",
            Registry::default().with(&Service(false)).unwrap(),
        );
        let cx = rig.cx();
        let input = Json::default();
        let mut call = Box::pin(module.tools[0].execute(&input, &cx));
        let mut context = std::task::Context::from_waker(Waker::noop());
        assert!(call.as_mut().poll(&mut context).is_pending());
        assert_eq!(locked(&module.tools[0].callbacks.calls).len(), 1);
        drop(call);
        assert!(locked(&module.tools[0].callbacks.calls).is_empty());
    }

    struct Wait;
    impl Tool for Wait {
        type In = ext::Args;
        type Out = bool;
        const NAME: &'static str = "wait";
        const DESCRIPTION: &'static str = "wait";
        async fn call(&self, args: &ext::Args, cx: &Cx) -> Result<bool, String> {
            if args.args == "park" {
                return std::future::pending().await;
            }
            cx.service::<ext::Flag>().unwrap().wait().await;
            Ok(true)
        }
    }

    struct Service(bool);
    impl Extension for Service {
        const NAME: &'static str = "service";
        const VERSION: u32 = 1;
        fn contribute(&self, r: &mut Registrar) {
            r.tool(Wait);
            r.provide(Arc::new(ext::Flag::default()));
            if self.0 {
                r.on::<ext::point::Permission>(|_, _| protocol::Permission::Deny);
            }
        }
    }

    #[test]
    fn delegated_effects_require_capability_and_policy_and_suspend_until_completion() {
        let source = tool("return cx.tool('wait','wait',{args=''})");
        let denied = Rig::new(
            "lua-no-grant",
            registry(&source, &[]).with(&Service(false)).unwrap(),
        );
        assert!(
            matches!(denied.call("test", &json!({})), Outcome::Failed { error } if error.contains("not granted"))
        );
        let denied = Rig::new(
            "lua-policy-deny",
            registry(&source, &["wait".into()])
                .with(&Service(true))
                .unwrap(),
        );
        assert!(
            matches!(denied.call("test", &json!({})), Outcome::Failed { error } if error.contains("denied"))
        );
        let rig = Rig::new(
            "lua-await",
            registry(&source, &["wait".into()])
                .with(&Service(false))
                .unwrap(),
        );
        let cx = rig.cx();
        let input = Json::encode(&json!({})).unwrap();
        let release = cx.service::<ext::Flag>().unwrap();
        let mut call = Box::pin(rig.registry.tools().run("test", &input, None, &cx));
        let mut context = std::task::Context::from_waker(Waker::noop());
        assert!(call.as_mut().poll(&mut context).is_pending());
        release.set();
        assert!(matches!(rt::block_on(call).outcome, Outcome::Done { .. }));
        assert_eq!(rig.done("test", &json!({})), true);
    }
}
