// SPDX-License-Identifier: GPL-3.0-or-later
//! Limits on incoming prompts, shared by the protocol backends (SPEC §9, §14): a few at once,
//! one per peer, and a cooldown for a peer whose request was declined or timed out.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// At most this many prompts wait at once, and one per peer (SPEC §9).
const MAX_PROMPTS: usize = 3;
const MAX_PROMPTS_PER_PEER: usize = 1;
/// A peer whose request was declined or timed out cannot ask again for this long (SPEC §14).
const REJECT_COOLDOWN: Duration = Duration::from_secs(30);

/// Pending prompts and cooldowns, by where requests come from.
pub struct Prompts<K> {
    inner: Mutex<Inner<K>>,
}

struct Inner<K> {
    pending: HashMap<K, usize>,
    cooldown: HashMap<K, Instant>,
}

impl<K: Clone + Eq + Hash> Prompts<K> {
    pub fn new() -> Arc<Self> {
        Arc::new(Prompts { inner: Mutex::new(Inner { pending: HashMap::new(), cooldown: HashMap::new() }) })
    }

    fn inner(&self) -> MutexGuard<'_, Inner<K>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Reserves a prompt for `origin`, or `None` when the limits or a cooldown say no.
    pub fn try_prompt(self: &Arc<Self>, origin: &K) -> Option<PromptSlot<K>> {
        let mut inner = self.inner();
        let now = Instant::now();
        inner.cooldown.retain(|_, until| *until > now);
        let total: usize = inner.pending.values().sum();
        let mine = inner.pending.get(origin).copied().unwrap_or(0);
        if inner.cooldown.contains_key(origin) || total >= MAX_PROMPTS || mine >= MAX_PROMPTS_PER_PEER {
            return None;
        }
        *inner.pending.entry(origin.clone()).or_default() += 1;
        Some(PromptSlot { prompts: self.clone(), origin: origin.clone() })
    }

    pub fn cool_down(&self, origin: &K) {
        self.inner().cooldown.insert(origin.clone(), Instant::now() + REJECT_COOLDOWN);
    }
}

/// A pending prompt, released when dropped.
pub struct PromptSlot<K: Clone + Eq + Hash> {
    prompts: Arc<Prompts<K>>,
    origin: K,
}

impl<K: Clone + Eq + Hash> Drop for PromptSlot<K> {
    fn drop(&mut self) {
        let mut inner = self.prompts.inner();
        if let Some(n) = inner.pending.get_mut(&self.origin) {
            *n -= 1;
            if *n == 0 {
                inner.pending.remove(&self.origin);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_and_cooldown() {
        let prompts = Prompts::new();
        let a = prompts.try_prompt(&"a").unwrap();
        assert!(prompts.try_prompt(&"a").is_none(), "one per peer");
        let b = prompts.try_prompt(&"b").unwrap();
        let _c = prompts.try_prompt(&"c").unwrap();
        assert!(prompts.try_prompt(&"d").is_none(), "three at once");
        drop(b);
        assert!(prompts.try_prompt(&"d").is_some());
        drop(a);
        prompts.cool_down(&"a");
        assert!(prompts.try_prompt(&"a").is_none(), "cooling down");
    }
}
