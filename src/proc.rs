//! Child-process bookkeeping so shutdown can stop everything the harness spawned.

use std::process::Child;
use std::sync::{Arc, Mutex, Weak};

static CHILDREN: Mutex<Vec<Weak<Mutex<Child>>>> = Mutex::new(Vec::new());

pub fn track(child: Child) -> Arc<Mutex<Child>> {
    let shared = Arc::new(Mutex::new(child));
    let mut children = CHILDREN.lock().unwrap();
    children.retain(|weak| weak.strong_count() > 0);
    children.push(Arc::downgrade(&shared));
    shared
}

/// Kill every tracked child that is still running.
pub fn kill_all() {
    let children: Vec<Arc<Mutex<Child>>> = CHILDREN.lock().unwrap().iter().filter_map(Weak::upgrade).collect();
    for child in children {
        if let Ok(mut child) = child.try_lock() {
            let _ = child.kill();
            let _ = child.wait();
        } else if let Ok(mut child) = child.lock() {
            let _ = child.kill();
        }
    }
}
