use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bevy::prelude::*;
use kuluu_render::hud::macros::{default_macro_book, MACRO_BOOKS};
use kuluu_render::MacroBook;
use serde::{Deserialize, Serialize};

#[derive(Resource, Debug, Clone)]
pub struct MacroStoreRes {
    pub store: MacroStore,
}

#[derive(Debug, Clone)]
pub struct MacroStore {
    path: PathBuf,
}

impl MacroStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn default_path() -> Result<PathBuf> {
        kuluu_session::config_dir::config_file("macros.json")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Option<BTreeMap<String, Books>>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                let books = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parse {}", self.path.display()))?;
                Ok(Some(books))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("read {}", self.path.display())),
        }
    }

    pub fn save(&self, books: &BTreeMap<String, Books>) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create dir {}", parent.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(books).context("serialize macros")?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), self.path.display()))?;
        Ok(())
    }
}

/// The full retail set of macro books for one character: `MACRO_BOOKS`
/// books, each with `MACRO_PAGES` pages of `MACROS_PER_PAGE` slots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Books {
    pub books: Vec<MacroBook>,
}

impl Default for Books {
    fn default() -> Self {
        Self {
            books: (0..MACRO_BOOKS).map(default_macro_book).collect(),
        }
    }
}

pub fn load_or_default() -> (BTreeMap<String, Books>, MacroStore) {
    let path = match MacroStore::default_path() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "macros: no config dir; using empty books");
            return (
                BTreeMap::new(),
                MacroStore::new(std::env::temp_dir().join("ffxi-macros.json")),
            );
        }
    };
    let store = MacroStore::new(path);
    match store.load() {
        Ok(Some(books)) => (books, store),
        Ok(None) => (BTreeMap::new(), store),
        Err(e) => {
            tracing::warn!(
                path = %store.path().display(),
                error = %e,
                "macros: parse failed; falling back to empty books",
            );
            (BTreeMap::new(), store)
        }
    }
}

pub fn persist_macros_on_change(books: Res<MacroBooks>, store: Res<MacroStoreRes>) {
    if !books.is_changed() {
        return;
    }
    if let Err(e) = store.store.save(&books.map) {
        tracing::warn!(
            path = %store.store.path().display(),
            error = %e,
            "macros: failed to persist",
        );
    }
}

/// Per-character macro books, keyed by character ID (string) or "default".
#[derive(Resource, Debug, Clone, Default)]
pub struct MacroBooks {
    pub map: BTreeMap<String, Books>,
}

impl MacroBooks {
    pub fn get_or_default(&mut self, char_key: &str) -> &Books {
        if !self.map.contains_key(char_key) {
            self.map.insert(char_key.to_string(), Books::default());
        }
        self.map.get(char_key).unwrap()
    }

    pub fn get_or_default_mut(&mut self, char_key: &str) -> &mut Books {
        if !self.map.contains_key(char_key) {
            self.map.insert(char_key.to_string(), Books::default());
        }
        self.map.get_mut(char_key).unwrap()
    }
}

/// Resolve the character key from the current snapshot's `self_char_id`.
pub fn char_key(self_char_id: Option<u32>) -> String {
    match self_char_id {
        Some(id) => id.to_string(),
        None => "default".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_path() -> PathBuf {
        let mut p = std::env::temp_dir();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        p.push(format!(
            "ffxi-macros-store-{}-{:?}-{stamp}.json",
            std::process::id(),
            std::thread::current().id(),
        ));
        p
    }

    #[test]
    fn default_path_uses_player_facing_dir() {
        let path = MacroStore::default_path().unwrap();
        assert!(
            path.ends_with("kuluu/macros.json"),
            "got {}",
            path.display()
        );
    }

    #[test]
    fn load_missing_returns_none() {
        let store = MacroStore::new(tmp_path());
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let store = MacroStore::new(tmp_path());
        let mut map = BTreeMap::new();
        let mut books = Books::default();
        books.books[3].pages[4].macros[2].lines[0] = "/heal".into();
        map.insert("12345".into(), books);
        store.save(&map).unwrap();
        let loaded = store.load().unwrap().expect("present after save");
        assert_eq!(
            loaded.get("12345").unwrap().books[3].pages[4].macros[2].lines[0],
            "/heal"
        );
        std::fs::remove_file(store.path()).ok();
    }

    #[test]
    fn char_key_some_uses_stringified_id() {
        assert_eq!(char_key(Some(42)), "42");
        assert_eq!(char_key(None), "default");
    }

    #[test]
    fn default_books_have_full_retail_shape() {
        let books = Books::default();
        assert_eq!(books.books.len(), MACRO_BOOKS);
        assert_eq!(books.books[0].name, "Book 1");
        assert_eq!(books.books[39].name, "Book 40");
        assert!(books.books[0].pages[0].macros[0].is_empty());
    }

    #[test]
    fn get_or_default_seeds_a_character() {
        let mut books = MacroBooks::default();
        assert_eq!(books.map.len(), 0);
        assert_eq!(books.get_or_default("7").books[0].name, "Book 1");
        assert_eq!(books.map.len(), 1);
        assert_eq!(books.get_or_default("7").books[0].name, "Book 1");
        assert_eq!(books.map.len(), 1);
    }
}
