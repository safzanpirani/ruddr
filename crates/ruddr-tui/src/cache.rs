//! Rendered lines cached per artifact block. A block re-renders only when
//! its key changes: the block's version (which its source bumps on every
//! visible change), the wrap width, the theme, the search query, whether it
//! holds the current search match, and an animation frame for the few blocks
//! that animate. Everything else is reused frame to frame.

use crate::text::{Row, wrap_rows};
use ratatui::style::Style;
use ratatui::text::Line;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub version: u64,
    pub width: u16,
    pub theme: usize,
    pub query: u64,
    pub current: bool,
    pub anim: u64,
}

pub fn hash_query(query: &str) -> u64 {
    query
        .to_lowercase()
        .bytes()
        .fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

#[derive(Default)]
pub struct RenderCache {
    scope: String,
    entries: HashMap<u64, (Key, Vec<Line<'static>>)>,
    frame: u64,
    used: HashMap<u64, u64>,
    pub renders: u64,
    pub hits: u64,
}

impl RenderCache {
    /// Drops everything when the scope (session and tab) changes.
    pub fn scope(&mut self, scope: &str) {
        if self.scope != scope {
            self.scope = scope.to_string();
            self.entries.clear();
            self.used.clear();
        }
    }

    pub fn begin_frame(&mut self) {
        self.frame += 1;
        // Forget blocks that left the view a while ago.
        if self.entries.len() > 64 && self.frame.is_multiple_of(120) {
            let frame = self.frame;
            let used = &self.used;
            self.entries.retain(|id, _| used.get(id).is_some_and(|f| frame - f < 120));
            self.used.retain(|_, f| frame - *f < 120);
        }
    }

    /// The lines cached for block `id` this frame.
    pub fn peek(&self, id: u64) -> Option<&[Line<'static>]> {
        self.entries.get(&id).map(|(_, lines)| lines.as_slice())
    }

    /// The wrapped lines of block `id`, rendering them only on a key change.
    pub fn lines(&mut self, id: u64, key: Key, query: &str, mark: Style, render: impl FnOnce() -> Vec<Row>) -> &[Line<'static>] {
        self.used.insert(id, self.frame);
        let fresh = self.entries.get(&id).is_some_and(|(k, _)| *k == key);
        if fresh {
            self.hits += 1;
        } else {
            self.renders += 1;
            let rows = render();
            let lines = wrap_rows(&rows, key.width as usize, query, mark);
            self.entries.insert(id, (key, lines));
        }
        &self.entries[&id].1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(version: u64, width: u16) -> Key {
        Key {
            version,
            width,
            theme: 0,
            query: 0,
            current: false,
            anim: 0,
        }
    }

    #[test]
    fn renders_once_per_key() {
        let mut cache = RenderCache::default();
        cache.scope("a");
        let mut calls = 0;
        for _ in 0..3 {
            let lines = cache.lines(1, key(1, 20), "", Style::new(), || {
                calls += 1;
                vec![Row::new(Line::from("hello world"))]
            });
            assert_eq!(lines.len(), 1);
        }
        assert_eq!(calls, 1);
        assert_eq!((cache.renders, cache.hits), (1, 2));
        // A new version, a new width, or a new scope renders again.
        cache.lines(1, key(2, 20), "", Style::new(), || vec![Row::new(Line::from("hello world"))]);
        let wrapped = cache
            .lines(1, key(2, 6), "", Style::new(), || vec![Row::new(Line::from("hello world"))])
            .len();
        assert_eq!(wrapped, 2);
        cache.scope("b");
        cache.lines(1, key(2, 6), "", Style::new(), || vec![Row::new(Line::from("x"))]);
        assert_eq!(cache.renders, 4);
    }

    #[test]
    fn other_blocks_stay_cached_while_one_streams() {
        let mut cache = RenderCache::default();
        cache.scope("s");
        for version in 1..=10 {
            for id in 0..50u64 {
                let v = if id == 49 { version } else { 1 };
                cache.lines(id, key(v, 40), "", Style::new(), || vec![Row::new(Line::from("text"))]);
            }
        }
        assert_eq!(cache.renders, 50 + 9, "only the streaming block re-renders");
    }
}
