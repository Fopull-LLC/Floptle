//! Where an extension keeps things between frames, between sessions, and
//! between machines.
//!
//! Three stores, because "remember this" means three different things and a
//! single one always ends up holding all three badly:
//!
//! | | `ed.prefs` | `ed.store` | `ed.session` |
//! |---|---|---|---|
//! | scope | this person, every project | this project, everybody | until the editor quits |
//! | lives in | the editor's config folder | `<project>/.floptle/packages/` | memory |
//! | commit it? | no — it is not the project's | yes, if the project wants it shared | n/a |
//!
//! An API key belongs in `prefs`. A per-scene annotation belongs in `store`. A
//! "have I already asked about this?" flag belongs in `session`, because the
//! answer should not survive a restart.
//!
//! Values are strings, numbers and booleans. Anything structured goes through
//! `json.encode` — which keeps the file readable and means a package cannot
//! wedge the editor by storing a table that refers to itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What a store can hold.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum Value {
    Text(String),
    Num(f64),
    Bool(bool),
}

impl Value {
    pub(crate) fn to_lua(&self, lua: &mlua::Lua) -> mlua::Result<mlua::Value> {
        Ok(match self {
            Value::Text(s) => mlua::Value::String(lua.create_string(s)?),
            Value::Num(n) => mlua::Value::Number(*n),
            Value::Bool(b) => mlua::Value::Boolean(*b),
        })
    }

    pub(crate) fn from_lua(v: &mlua::Value) -> Option<Value> {
        match v {
            mlua::Value::String(s) => Some(Value::Text(s.to_string_lossy().to_string())),
            mlua::Value::Integer(i) => Some(Value::Num(*i as f64)),
            mlua::Value::Number(n) => Some(Value::Num(*n)),
            mlua::Value::Boolean(b) => Some(Value::Bool(*b)),
            _ => None,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Doc {
    #[serde(default)]
    values: HashMap<String, Value>,
}

/// One package's three stores.
#[derive(Default)]
pub(crate) struct Store {
    user: HashMap<String, Value>,
    project: HashMap<String, Value>,
    session: HashMap<String, Value>,
    user_path: Option<PathBuf>,
    project_path: Option<PathBuf>,
    /// Nothing is written until something changes — opening ten packages must
    /// not rewrite ten files.
    user_dirty: bool,
    project_dirty: bool,
}

/// Which of the three.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    User,
    Project,
    Session,
}

impl Store {
    fn map(&self, k: Kind) -> &HashMap<String, Value> {
        match k {
            Kind::User => &self.user,
            Kind::Project => &self.project,
            Kind::Session => &self.session,
        }
    }

    pub(crate) fn get(&self, k: Kind, key: &str) -> Option<&Value> {
        self.map(k).get(key)
    }

    pub(crate) fn set(&mut self, k: Kind, key: String, v: Option<Value>) {
        let (map, dirty) = match k {
            Kind::User => (&mut self.user, Some(&mut self.user_dirty)),
            Kind::Project => (&mut self.project, Some(&mut self.project_dirty)),
            Kind::Session => (&mut self.session, None),
        };
        let changed = match v {
            Some(v) => map.insert(key, v).is_some_and(|_| true) || true,
            None => map.remove(&key).is_some(),
        };
        if changed && let Some(d) = dirty {
            *d = true;
        }
    }

    pub(crate) fn keys(&self, k: Kind) -> Vec<String> {
        let mut v: Vec<String> = self.map(k).keys().cloned().collect();
        v.sort();
        v
    }

    fn save(&mut self) {
        if self.user_dirty && let Some(p) = self.user_path.clone() {
            write(&p, &self.user);
            self.user_dirty = false;
        }
        if self.project_dirty && let Some(p) = self.project_path.clone() {
            write(&p, &self.project);
            self.project_dirty = false;
        }
    }
}

fn write(path: &Path, map: &HashMap<String, Value>) {
    let doc = Doc { values: map.clone() };
    let cfg = ron::ser::PrettyConfig::new().struct_names(false);
    let Ok(text) = ron::ser::to_string_pretty(&doc, cfg) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, format!("{text}\n"));
}

fn read(path: &Path) -> HashMap<String, Value> {
    let Ok(text) = std::fs::read_to_string(path) else { return HashMap::new() };
    ron::from_str::<Doc>(&text).map(|d| d.values).unwrap_or_default()
}

/// Every loaded package's stores.
#[derive(Default)]
pub(crate) struct Stores {
    by_pkg: HashMap<String, Store>,
}

impl Stores {
    /// Open (or reopen) a package's stores, reading both files.
    pub(crate) fn open(&mut self, id: &str, project_root: &Path) {
        // A package id is reverse-DNS with no path separators (the manifest
        // validator guarantees it), so it is safe as a file name.
        let user_path = crate::prefs::floptle_config_dir()
            .map(|d| d.join("packages").join(format!("{id}.ron")));
        let project_path = Some(project_root.join(".floptle/packages").join(format!("{id}.ron")));
        let user = user_path.as_ref().map(|p| read(p)).unwrap_or_default();
        let project = project_path.as_ref().map(|p| read(p)).unwrap_or_default();
        // Session survives a package RELOAD (it is "until the editor quits"),
        // so it is carried over rather than rebuilt.
        let session = self.by_pkg.remove(id).map(|s| s.session).unwrap_or_default();
        self.by_pkg.insert(
            id.to_string(),
            Store {
                user,
                project,
                session,
                user_path,
                project_path,
                user_dirty: false,
                project_dirty: false,
            },
        );
    }

    pub(crate) fn get(&self, id: &str) -> Option<&Store> {
        self.by_pkg.get(id)
    }

    pub(crate) fn entry(&mut self, id: &str) -> &mut Store {
        self.by_pkg.entry(id.to_string()).or_default()
    }

    pub(crate) fn save_all(&mut self) {
        for s in self.by_pkg.values_mut() {
            s.save();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_store_round_trips_through_a_file() {
        let dir = std::env::temp_dir().join(format!("flext-prefs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.ron");
        let mut m = HashMap::new();
        m.insert("key".to_string(), Value::Text("abc".into()));
        m.insert("n".to_string(), Value::Num(4.5));
        m.insert("on".to_string(), Value::Bool(true));
        write(&path, &m);
        assert_eq!(read(&path), m);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn setting_nothing_removes_the_key() {
        let mut s = Store::default();
        s.set(Kind::Session, "a".into(), Some(Value::Bool(true)));
        assert!(s.get(Kind::Session, "a").is_some());
        s.set(Kind::Session, "a".into(), None);
        assert!(s.get(Kind::Session, "a").is_none());
    }

    #[test]
    fn the_three_stores_do_not_see_each_other() {
        let mut s = Store::default();
        s.set(Kind::User, "k".into(), Some(Value::Num(1.0)));
        s.set(Kind::Project, "k".into(), Some(Value::Num(2.0)));
        s.set(Kind::Session, "k".into(), Some(Value::Num(3.0)));
        assert_eq!(s.get(Kind::User, "k"), Some(&Value::Num(1.0)));
        assert_eq!(s.get(Kind::Project, "k"), Some(&Value::Num(2.0)));
        assert_eq!(s.get(Kind::Session, "k"), Some(&Value::Num(3.0)));
    }

    /// A reload replaces a package's code; it must not lose the "already asked"
    /// flags the running session accumulated.
    #[test]
    fn session_survives_a_package_reload() {
        let dir = std::env::temp_dir().join(format!("flext-reload-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut stores = Stores::default();
        stores.open("com.t.a", &dir);
        stores.entry("com.t.a").set(Kind::Session, "seen".into(), Some(Value::Bool(true)));
        stores.open("com.t.a", &dir);
        assert_eq!(stores.get("com.t.a").unwrap().get(Kind::Session, "seen"), Some(&Value::Bool(true)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_project_store_lands_inside_the_project() {
        let dir = std::env::temp_dir().join(format!("flext-proj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut stores = Stores::default();
        stores.open("com.t.a", &dir);
        stores.entry("com.t.a").set(Kind::Project, "k".into(), Some(Value::Text("v".into())));
        stores.save_all();
        assert!(dir.join(".floptle/packages/com.t.a.ron").exists());
        // …and reading it back finds the value.
        let mut fresh = Stores::default();
        fresh.open("com.t.a", &dir);
        assert_eq!(
            fresh.get("com.t.a").unwrap().get(Kind::Project, "k"),
            Some(&Value::Text("v".into()))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_is_written_until_something_changes() {
        let dir = std::env::temp_dir().join(format!("flext-clean-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut stores = Stores::default();
        stores.open("com.t.a", &dir);
        stores.save_all();
        assert!(!dir.join(".floptle/packages/com.t.a.ron").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
