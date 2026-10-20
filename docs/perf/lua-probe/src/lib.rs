use std::path::Path;

pub fn run(home: &Path) {
    let Ok(source) = std::fs::read_to_string(home.join("probe.lua")) else { return };
    #[cfg(not(feature = "pure"))]
    {
        let lua = mlua::Lua::new();
        lua.load(&source).exec().unwrap();
        let handler: mlua::Function = lua.globals().get("handler").unwrap();
        let output: String = handler.call("{\"command\":\"hello\"}").unwrap();
        assert_eq!(output, "{\"replace\":{\"command\":\"hello\"}}");
        let tool: mlua::Function = lua.globals().get("tool").unwrap();
        let output: String = tool.call("{\"command\":\"hello\"}").unwrap();
        assert_eq!(output, "{\"command\":\"hello\"}");
        std::hint::black_box(lua.used_memory());
    }
    #[cfg(feature = "pure")]
    {
        let mut lua = piccolo::Lua::core();
        let executor = lua.try_enter(|ctx| {
            let closure = piccolo::Closure::load(ctx, None, source.as_bytes())?;
            Ok(ctx.stash(piccolo::Executor::start(ctx, closure.into(), ())))
        }).unwrap();
        lua.execute::<()>(&executor).unwrap();
        for (name, want) in [("handler", "{\"replace\":{\"command\":\"hello\"}}"), ("tool", "{\"command\":\"hello\"}")] {
            lua.try_enter(|ctx| {
                let function = piccolo::Function::from_value(ctx, ctx.get_global(name))?;
                ctx.fetch(&executor).restart(ctx, function, ("{\"command\":\"hello\"}",));
                Ok(())
            }).unwrap();
            let output: String = lua.execute(&executor).unwrap();
            assert_eq!(output, want);
        }
        std::hint::black_box(lua.total_memory());
    }
}

#[cfg(feature = "pure")]
use piccolo::FromValue;

use std::sync::{Arc, Mutex};
use ext::{DynExtension, Registrar, Name, DynTool, Parsed, Called, Cx, BoxFuture, Replay, Concurrency, PointCx};
use protocol::{Audience, Card, Json, Outcome};

pub struct Mod(Arc<Vm>);

#[cfg(not(feature = "pure"))]
struct Vm(mlua::Lua);
#[cfg(feature = "pure")]
struct Vm(Mutex<std::sync::mpsc::Sender<(String, String, std::sync::mpsc::SyncSender<String>)>>);

impl Vm {
    fn new(source: String) -> Self {
        #[cfg(not(feature = "pure"))]
        {
            let lua = mlua::Lua::new();
            lua.load(&source).exec().unwrap();
            Self(lua)
        }
        #[cfg(feature = "pure")]
        {
            let (tx, rx) = std::sync::mpsc::channel::<(String, String, std::sync::mpsc::SyncSender<String>)>();
            std::thread::spawn(move || {
                let mut lua = piccolo::Lua::core();
                let executor = lua.try_enter(|ctx| {
                    let closure = piccolo::Closure::load(ctx, None, source.as_bytes())?;
                    Ok(ctx.stash(piccolo::Executor::start(ctx, closure.into(), ())))
                }).unwrap();
                lua.execute::<()>(&executor).unwrap();
                for (name, input, reply) in rx {
                    lua.try_enter(|ctx| {
                        let function = piccolo::Function::from_value(ctx, ctx.get_global(ctx.intern(name.as_bytes())))?;
                        ctx.fetch(&executor).restart(ctx, function, (ctx.intern(input.as_bytes()),));
                        Ok(())
                    }).unwrap();
                    reply.send(lua.execute::<String>(&executor).unwrap()).unwrap();
                }
            });
            Self(Mutex::new(tx))
        }
    }
    fn call(&self, name: &str, input: &str) -> String {
        #[cfg(not(feature = "pure"))]
        { self.0.globals().get::<mlua::Function>(name).unwrap().call(input).unwrap() }
        #[cfg(feature = "pure")]
        {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            self.0.lock().unwrap().send((name.into(), input.into(), tx)).unwrap();
            rx.recv().unwrap()
        }
    }
}

pub fn load(home: &Path) -> Option<Mod> {
    let source = std::fs::read_to_string(home.join("ext/probe.lua")).ok()?;
    Some(Mod(Arc::new(Vm::new(source))))
}

impl DynExtension for Mod {
    fn name(&self) -> Name { Name::from("probe") }
    fn version(&self) -> u32 { 1 }
    fn contribute(&self, r: &mut Registrar) {
        r.on_erased("input", Arc::new(Self(Arc::clone(&self.0))));
        r.dyn_tool(Arc::new(Self(Arc::clone(&self.0))));
    }
}
impl ext::point::ErasedHandler for Mod {
    fn on(&self, _: &PointCx<'_>, args: &Json) -> Json {
        serde_json::from_str::<Json>(&self.0.call("handler", args.raw())).unwrap()
    }
}
impl DynTool for Mod {
    fn name(&self) -> &str { "probe" }
    fn description(&self) -> &str { "Calls the Lua probe." }
    fn audience(&self) -> Audience { Audience::User }
    fn replay(&self) -> Replay { Replay::Unsafe }
    fn concurrency(&self) -> Concurrency { Concurrency::Shared }
    fn spec(&self) -> llm::Tool { llm::Tool { name: "probe".into(), description: self.description().into(), schema: serde_json::json!({"type":"object"}), strict: None } }
    fn parse(&self, input: &Json) -> Result<Parsed, String> { Ok(Parsed::new(input.clone())) }
    fn call<'a>(&'a self, input: Parsed, _: &'a Cx) -> BoxFuture<'a, Called> {
        let output = serde_json::from_str::<Json>(&self.0.call("tool", input.get::<Json>().unwrap().raw())).unwrap();
        Box::pin(std::future::ready(Called { outcome: Outcome::Done { output }, card: None }))
    }
    fn to_model(&self, output: &Json) -> llm::Output { llm::Output::Json(output.clone()) }
    fn present(&self, _: &Parsed) -> Option<Card> { None }
}
