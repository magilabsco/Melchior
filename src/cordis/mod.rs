//! A Rust port of the Cordis plugin model.
//!
//! Cordis organises an application as a tree of *scopes*. Every plugin runs in
//! its own scope and receives a [`Context`] bound to it. Everything a plugin
//! does through its context (providing a service, registering a tool,
//! listening to an event, declaring a cleanup effect) is owned by that scope
//! and is undone automatically, in reverse order, when the scope is disposed.
//!
//! Services are named, typed values shared between plugins. A plugin lists the
//! services it needs in [`Plugin::inject`]; the loader activates it only once
//! they all exist, and disposes it again if one of them goes away. That gives
//! the same "plugins wait for their dependencies" behaviour as JavaScript
//! Cordis without runtime reflection.

pub mod loader;

use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::json::Json;

pub type Dispose = Box<dyn FnOnce() + Send>;
type Handler = Arc<dyn Fn(&Json) + Send + Sync>;

pub trait Plugin: Send + Sync {
    /// Stable plugin name, e.g. `tool-bash`.
    fn name(&self) -> &'static str;
    /// Services that must exist before `apply` runs.
    fn inject(&self) -> &'static [&'static str] {
        &[]
    }
    fn apply(&self, ctx: &Context) -> Result<(), String>;
}

struct ServiceSlot {
    value: Arc<dyn Any + Send + Sync>,
    owner: u64,
}

struct Listener {
    event: String,
    owner: u64,
    handler: Handler,
}

pub struct Scope {
    id: u64,
    name: String,
    entry: String,
    config: Json,
    injects: Vec<String>,
    disposables: Mutex<Vec<Dispose>>,
    active: AtomicBool,
}

impl Scope {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn entry(&self) -> &str {
        &self.entry
    }
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
}

struct AppInner {
    services: RwLock<HashMap<String, ServiceSlot>>,
    listeners: RwLock<Vec<Listener>>,
    scopes: Mutex<Vec<Arc<Scope>>>,
    next_id: AtomicU64,
}

#[derive(Clone)]
pub struct App {
    inner: Arc<AppInner>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> App {
        App {
            inner: Arc::new(AppInner {
                services: RwLock::new(HashMap::new()),
                listeners: RwLock::new(Vec::new()),
                scopes: Mutex::new(Vec::new()),
                next_id: AtomicU64::new(1),
            }),
        }
    }

    /// The root context. Services provided here live for the whole process.
    pub fn root(&self) -> Context {
        let scope = self.new_scope("root", "root", Json::obj(), Vec::new());
        Context { app: self.clone(), scope }
    }

    fn new_scope(&self, name: &str, entry: &str, config: Json, injects: Vec<String>) -> Arc<Scope> {
        let scope = Arc::new(Scope {
            id: self.inner.next_id.fetch_add(1, Ordering::SeqCst),
            name: name.to_string(),
            entry: entry.to_string(),
            config,
            injects,
            disposables: Mutex::new(Vec::new()),
            active: AtomicBool::new(true),
        });
        self.inner.scopes.lock().unwrap().push(scope.clone());
        scope
    }

    pub fn has_service(&self, name: &str) -> bool {
        self.inner.services.read().unwrap().contains_key(name)
    }

    pub fn get<T: Send + Sync + 'static>(&self, name: &str) -> Option<Arc<T>> {
        let value = self.inner.services.read().unwrap().get(name).map(|slot| slot.value.clone())?;
        value.downcast::<T>().ok()
    }

    pub fn service_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.inner.services.read().unwrap().keys().cloned().collect();
        names.sort();
        names
    }

    pub fn active_scopes(&self) -> Vec<Arc<Scope>> {
        self.inner.scopes.lock().unwrap().iter().filter(|s| s.is_active()).cloned().collect()
    }

    pub fn emit(&self, event: &str, payload: &Json) {
        let handlers: Vec<Handler> = self
            .inner
            .listeners
            .read()
            .unwrap()
            .iter()
            .filter(|l| l.event == event)
            .map(|l| l.handler.clone())
            .collect();
        for handler in handlers {
            handler(payload);
        }
    }

    /// Dispose a scope: run its effects in reverse order, withdraw its
    /// services and listeners, then dispose every scope that injected one of
    /// the withdrawn services.
    pub fn dispose(&self, scope: &Arc<Scope>) {
        if !scope.active.swap(false, Ordering::SeqCst) {
            return;
        }
        let effects: Vec<Dispose> = std::mem::take(&mut *scope.disposables.lock().unwrap());
        for effect in effects.into_iter().rev() {
            effect();
        }
        let withdrawn: Vec<String> = {
            let mut services = self.inner.services.write().unwrap();
            let names: Vec<String> =
                services.iter().filter(|(_, slot)| slot.owner == scope.id).map(|(name, _)| name.clone()).collect();
            for name in &names {
                services.remove(name);
            }
            names
        };
        self.inner.listeners.write().unwrap().retain(|l| l.owner != scope.id);
        self.inner.scopes.lock().unwrap().retain(|s| s.id != scope.id);
        if !withdrawn.is_empty() {
            let dependents: Vec<Arc<Scope>> = self
                .inner
                .scopes
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.is_active() && s.injects.iter().any(|i| withdrawn.contains(i)))
                .cloned()
                .collect();
            for dependent in dependents {
                crate::log_info!("cordis", "disposing {} after its dependency was withdrawn", dependent.name);
                self.dispose(&dependent);
            }
        }
    }

    /// Dispose every scope, most recently created first.
    pub fn dispose_all(&self) {
        loop {
            let last = self.inner.scopes.lock().unwrap().last().cloned();
            match last {
                Some(scope) => self.dispose(&scope),
                None => return,
            }
        }
    }
}

#[derive(Clone)]
pub struct Context {
    app: App,
    scope: Arc<Scope>,
}

impl Context {
    pub fn app(&self) -> &App {
        &self.app
    }

    pub fn scope(&self) -> &Arc<Scope> {
        &self.scope
    }

    pub fn config(&self) -> &Json {
        &self.scope.config
    }

    pub fn name(&self) -> &str {
        &self.scope.name
    }

    /// Publish a service owned by this scope.
    pub fn provide<T: Send + Sync + 'static>(&self, name: &str, value: Arc<T>) -> Result<(), String> {
        let mut services = self.app.inner.services.write().unwrap();
        if services.contains_key(name) {
            return Err(format!("service {name:?} is already provided"));
        }
        services.insert(name.to_string(), ServiceSlot { value, owner: self.scope.id });
        Ok(())
    }

    pub fn get<T: Send + Sync + 'static>(&self, name: &str) -> Option<Arc<T>> {
        self.app.get(name)
    }

    pub fn inject<T: Send + Sync + 'static>(&self, name: &str) -> Result<Arc<T>, String> {
        self.get(name).ok_or_else(|| format!("{}: required service {name:?} is not available", self.scope.name))
    }

    /// Register cleanup that runs when this scope is disposed.
    pub fn effect(&self, dispose: impl FnOnce() + Send + 'static) {
        if self.scope.is_active() {
            self.scope.disposables.lock().unwrap().push(Box::new(dispose));
        } else {
            dispose();
        }
    }

    pub fn on(&self, event: &str, handler: impl Fn(&Json) + Send + Sync + 'static) {
        self.app.inner.listeners.write().unwrap().push(Listener {
            event: event.to_string(),
            owner: self.scope.id,
            handler: Arc::new(handler),
        });
    }

    pub fn emit(&self, event: &str, payload: &Json) {
        self.app.emit(event, payload);
    }

    /// Run a plugin in a child scope that is disposed with this one.
    pub fn plugin(&self, plugin: &dyn Plugin, entry: &str, config: Json) -> Result<Context, String> {
        for name in plugin.inject() {
            if !self.app.has_service(name) {
                return Err(format!("{}: required service {name:?} is not available", plugin.name()));
            }
        }
        let injects = plugin.inject().iter().map(|s| s.to_string()).collect();
        let scope = self.app.new_scope(plugin.name(), entry, config, injects);
        let child = Context { app: self.app.clone(), scope: scope.clone() };
        if let Err(error) = plugin.apply(&child) {
            self.app.dispose(&scope);
            return Err(error);
        }
        let app = self.app.clone();
        let weak = Arc::downgrade(&scope);
        self.effect(move || {
            if let Some(scope) = weak.upgrade() {
                app.dispose(&scope);
            }
        });
        Ok(child)
    }

    pub fn dispose(&self) {
        self.app.dispose(&self.scope);
    }
}

/// A list owned by one service whose entries belong to the scopes that added
/// them: an entry disappears when its scope is disposed.
pub struct Registry<T> {
    items: Mutex<Vec<(u64, String, Arc<T>)>>,
    next: AtomicU64,
}

impl<T: Send + Sync + 'static> Default for Registry<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Send + Sync + 'static> Registry<T> {
    pub fn new() -> Self {
        Registry { items: Mutex::new(Vec::new()), next: AtomicU64::new(1) }
    }

    pub fn add(self: &Arc<Self>, ctx: &Context, key: &str, item: T) -> Result<(), String> {
        let mut items = self.items.lock().unwrap();
        if items.iter().any(|(_, k, _)| k == key) {
            return Err(format!("{key:?} is already registered"));
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        items.push((id, key.to_string(), Arc::new(item)));
        drop(items);
        let registry = Arc::downgrade(self);
        ctx.effect(move || {
            if let Some(registry) = registry.upgrade() {
                registry.items.lock().unwrap().retain(|(i, _, _)| *i != id);
            }
        });
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<Arc<T>> {
        self.items.lock().unwrap().iter().find(|(_, k, _)| k == key).map(|(_, _, v)| v.clone())
    }

    pub fn all(&self) -> Vec<(String, Arc<T>)> {
        self.items.lock().unwrap().iter().map(|(_, k, v)| (k.clone(), v.clone())).collect()
    }

    pub fn len(&self) -> usize {
        self.items.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct Provider;
    impl Plugin for Provider {
        fn name(&self) -> &'static str {
            "provider"
        }
        fn apply(&self, ctx: &Context) -> Result<(), String> {
            ctx.provide("greeting", Arc::new(String::from("hi")))
        }
    }

    struct Consumer(Arc<AtomicUsize>);
    impl Plugin for Consumer {
        fn name(&self) -> &'static str {
            "consumer"
        }
        fn inject(&self) -> &'static [&'static str] {
            &["greeting"]
        }
        fn apply(&self, ctx: &Context) -> Result<(), String> {
            let greeting: Arc<String> = ctx.inject("greeting")?;
            assert_eq!(greeting.as_str(), "hi");
            let disposed = self.0.clone();
            ctx.effect(move || {
                disposed.fetch_add(1, Ordering::SeqCst);
            });
            Ok(())
        }
    }

    #[test]
    fn effects_run_and_dependents_dispose_with_their_service() {
        let app = App::new();
        let root = app.root();
        let disposed = Arc::new(AtomicUsize::new(0));
        let provider = root.plugin(&Provider, "p", Json::obj()).unwrap();
        let consumer = root.plugin(&Consumer(disposed.clone()), "c", Json::obj()).unwrap();
        assert!(consumer.scope().is_active());
        provider.dispose();
        assert!(!app.has_service("greeting"));
        assert!(!consumer.scope().is_active());
        assert_eq!(disposed.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn missing_injection_is_refused_and_registry_entries_are_scoped() {
        let app = App::new();
        let root = app.root();
        assert!(root.plugin(&Consumer(Arc::new(AtomicUsize::new(0))), "c", Json::obj()).is_err());
        let registry: Arc<Registry<u32>> = Arc::new(Registry::new());
        let provider = root.plugin(&Provider, "p", Json::obj()).unwrap();
        registry.add(&provider, "one", 1).unwrap();
        assert!(registry.add(&provider, "one", 2).is_err());
        assert_eq!(registry.len(), 1);
        provider.dispose();
        assert!(registry.is_empty());
    }

    #[test]
    fn listeners_are_removed_with_scope() {
        let app = App::new();
        let root = app.root();
        let hits = Arc::new(AtomicUsize::new(0));
        let child = root.plugin(&Provider, "p", Json::obj()).unwrap();
        let counter = hits.clone();
        child.on("ping", move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        app.emit("ping", &Json::Null);
        child.dispose();
        app.emit("ping", &Json::Null);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}
