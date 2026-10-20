use std::future::{Future, poll_fn};
use std::pin::pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ext::point::ErasedHandler;
use ext::{
    BoxFuture, Called, Concurrency, Cx, DynExtension, DynFold, DynTool, Name, Parsed, PointCx,
    Refused, Registrar, Replay, Text,
};
use mlua::chunk::ChunkMode;
use mlua::{Function, HookTriggers, Lua, LuaSerdeExt, StdLib, Table, Value, VmState};
use protocol::{Audience, Card, Json, Outcome};

#[derive(Clone, Copy)]
pub struct Limits {
    pub memory: usize,
    pub instructions: u64,
    pub slice: Duration,
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

struct Budget {
    left: AtomicU64,
    until: Mutex<Instant>,
    slice: Duration,
}

impl Budget {
    fn new(limits: Limits) -> Arc<Self> {
        Arc::new(Self {
            left: AtomicU64::new(limits.instructions),
            until: Mutex::new(Instant::now() + limits.slice),
            slice: limits.slice,
        })
    }

    fn reset_slice(&self) {
        *self.until.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now() + self.slice;
    }

    fn check(&self) -> mlua::Result<VmState> {
        let left = self
            .left
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                left.checked_sub(1000)
            });
        if left.is_err()
            || Instant::now() >= *self.until.lock().unwrap_or_else(PoisonError::into_inner)
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
    name: Name,
    version: u32,
    tools: Vec<Arc<LuaTool>>,
    hooks: Vec<(String, Arc<Hook>)>,
    texts: Vec<Text>,
    folds: Vec<Arc<LuaFold>>,
    agents: Vec<(String, LuaAgent)>,
}

#[derive(Default)]
struct Contributions {
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

fn sandbox(limits: Limits) -> mlua::Result<(Lua, Callbacks)> {
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
    Ok((lua, callbacks))
}

fn readonly(lua: &Lua, table: Table) -> mlua::Result<Table> {
    let view = lua.create_table()?;
    let meta = lua.create_table()?;
    meta.set("__index", table)?;
    meta.set(
        "__newindex",
        lua.create_function(|_, _: (Value, Value, Value)| -> mlua::Result<()> {
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

fn register(lua: &Lua) -> mlua::Result<Arc<Mutex<Contributions>>> {
    let made = Arc::new(Mutex::new(Contributions::default()));
    let api = lua.create_table()?;
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
            made.folds.push(spec);
            Ok(())
        })?,
    )?;
    api.set("null", lua.null())?;
    lua.globals().set("a", readonly(lua, api)?)?;
    Ok(made)
}

impl Mod {
    /// Compiles one generation; registration is transactional.
    pub fn load(
        name: &str,
        source: &str,
        tools: &[String],
        limits: Limits,
    ) -> Result<Self, Refused> {
        Self::compile(name, source, tools, limits, false)
            .map_err(|why| Refused(format!("lua {name}: {why}")))
    }

    fn compile(
        name: &str,
        source: &str,
        tools: &[String],
        limits: Limits,
        fold_only: bool,
    ) -> mlua::Result<Self> {
        if limits.memory == 0 || limits.instructions < 1000 || limits.slice.is_zero() {
            return Err(mlua::Error::runtime(
                "lua limits must be positive (at least 1000 instructions)",
            ));
        }
        let (lua, callbacks) = sandbox(limits)?;
        let made = register(&lua)?;
        let budget = Budget::new(limits);
        let thread = lua.create_thread(
            lua.load(source)
                .set_name(name)
                .set_mode(ChunkMode::Text)
                .into_function()?,
        )?;
        thread.set_hook(
            HookTriggers::new().every_nth_instruction(1000),
            move |_, _| budget.check(),
        )?;
        thread.resume::<()>(())?;
        let mut made = std::mem::take(&mut *locked(&made));
        let name = Name::from(name);
        let grants: Arc<[String]> = tools.to_vec().into();
        let mut result = Self {
            name: Arc::clone(&name),
            version: fingerprint(source),
            tools: Vec::new(),
            hooks: Vec::new(),
            texts: std::mem::take(&mut made.texts),
            folds: Vec::new(),
            agents: Vec::new(),
        };
        if !fold_only {
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
        if !fold_only && !made.folds.is_empty() {
            // Folding happens under the session state lock. Its VM must never
            // share a lock with callbacks that read that session's state.
            result.folds = Self::compile(&name, source, tools, limits, true)?.folds;
        } else if fold_only {
            for spec in made.folds {
                result.folds.push(Arc::new(LuaFold {
                    name: Name::from(text_field(&spec, "name", 128)?),
                    initial: encode(&spec.raw_get("initial")?)?,
                    run: spec.raw_get("apply")?,
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
                decode(&self.lua, &input).map_err(|e| e.to_string())?,
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

fn sync(
    lua: &Lua,
    run: &Function,
    args: impl mlua::IntoLuaMulti,
    limits: Limits,
) -> mlua::Result<Json> {
    let budget = Budget::new(limits);
    let thread = lua.create_thread(run.clone())?;
    thread.set_hook(
        HookTriggers::new().every_nth_instruction(1000),
        move |_, _| budget.check(),
    )?;
    encode(&thread.resume(args)?)
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
                    (decode(&self.lua, args)?, context),
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
    run: Function,
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
        (|| {
            sync(
                &self.lua,
                &self.run,
                (decode(&self.lua, state)?, decode(&self.lua, change)?),
                self.limits,
            )
        })()
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
    tool: String,
    quota: Arc<Quota>,
}
impl mlua::UserData for Call {}

#[derive(Clone)]
struct Callbacks {
    update: Function,
    invoke: Function,
}
impl Callbacks {
    fn new(lua: &Lua) -> mlua::Result<Self> {
        let update = lua.create_async_function(
            |_lua, (data, name, value): (mlua::AnyUserData, String, Value)| async move {
                let data = data.borrow::<Call>()?.clone();
                let cx = data
                    .cx
                    .upgrade()
                    .ok_or_else(|| mlua::Error::runtime("call context expired"))?;
                if cx.state_erased(&data.owner, &name).is_none() {
                    return Err(mlua::Error::runtime("no fold of that name"));
                }
                let change = encode(&value)?;
                data.quota.take(change.raw().len())?;
                cx.update_erased(&data.owner, &name, change).await;
                Ok(())
            },
        )?;
        let invoke = lua.create_async_function(
            |lua, (data, name, value): (mlua::AnyUserData, String, Value)| async move {
                let data = data.borrow::<Call>()?.clone();
                if name == data.tool || !data.grants.contains(&name) {
                    return Err(mlua::Error::runtime(format!(
                        "tool capability {name} is not granted"
                    )));
                }
                let cx = data
                    .cx
                    .upgrade()
                    .ok_or_else(|| mlua::Error::runtime("call context expired"))?;
                let input = encode(&value)?;
                data.quota.take(input.raw().len())?;
                match cx.invoke(&name, &input).await.outcome {
                    Outcome::Done { output } => decode(&lua, &output),
                    Outcome::Failed { error } => Err(mlua::Error::runtime(error)),
                    Outcome::Interrupted { .. } => {
                        Err(mlua::Error::runtime("delegated tool interrupted"))
                    }
                }
            },
        )?;
        Ok(Self { update, invoke })
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
        let data = self.lua.create_userdata(Call {
            cx: Arc::downgrade(&call),
            owner: Arc::clone(&self.owner),
            grants: Arc::clone(&self.grants),
            tool: self.name.clone(),
            quota: Arc::clone(&quota),
        })?;
        context.set("update", self.callbacks.update.bind(data.clone())?)?;
        context.set("tool", self.callbacks.invoke.bind(data)?)?;
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
        let budget = Budget::new(self.limits);
        let hook = Arc::clone(&budget);
        let thread = self.lua.create_thread(self.run.clone())?;
        thread.set_hook(
            HookTriggers::new().every_nth_instruction(1000),
            move |_, _| hook.check(),
        )?;
        let mut work = pin!(thread.into_async::<Value>((decode(&self.lua, input)?, context))?);
        let value = poll_fn(|cx| {
            budget.reset_slice();
            work.as_mut().poll(cx)
        })
        .await?;
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
                    outcome: Outcome::Done { output },
                    card: None,
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
            tool("return input")
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
                    .restore(&[ext::Image {
                        ext: "test".into(),
                        state: "count".into(),
                        image: json,
                    }])
                    .is_err()
            );
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

    struct Wait;
    impl Tool for Wait {
        type In = ext::Args;
        type Out = bool;
        const NAME: &'static str = "wait";
        const DESCRIPTION: &'static str = "wait";
        async fn call(&self, _: &ext::Args, cx: &Cx) -> Result<bool, String> {
            cx.service::<dyn rt::Runtime>()
                .unwrap()
                .sleep(Duration::from_millis(60))
                .await;
            Ok(true)
        }
    }

    struct Service(bool);
    impl Extension for Service {
        const NAME: &'static str = "service";
        const VERSION: u32 = 1;
        fn contribute(&self, r: &mut Registrar) {
            r.tool(Wait);
            if self.0 {
                r.on::<ext::point::Permission>(|_, _| protocol::Permission::Deny);
            }
        }
    }

    #[test]
    fn delegated_effects_require_a_capability_and_policy_and_yield_to_the_runtime() {
        let source = tool("return cx.tool('wait',{args=''})");
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
        let runtime = cx.service::<dyn rt::Runtime>().unwrap();
        let timer_won = rt::block_on(rt::or(
            async {
                rig.registry.tools().run("test", &input, None, &cx).await;
                false
            },
            async {
                runtime.sleep(Duration::from_millis(5)).await;
                true
            },
        ));
        assert!(timer_won);
        assert_eq!(rig.done("test", &json!({})), true);
    }
}
