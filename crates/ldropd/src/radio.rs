// SPDX-License-Identifier: GPL-3.0-or-later
//! One Wi-Fi card, two protocols: the Apple link (AWDL, through owlyd) and Quick Share's
//! network switches (joining or hosting a hotspot, Wi-Fi Direct) take the card in ways that
//! exclude each other, so each holds a claim while it does.

use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Use {
    AppleLink,
    WifiSwitch,
}

/// Who has the card. Many claims of one use can be held at once.
#[derive(Default)]
pub struct Radio {
    held: Mutex<Option<(Use, usize)>>,
}

impl Radio {
    /// A claim for `what`, or `None` while the card is in the other use.
    pub fn try_claim(self: &Arc<Self>, what: Use) -> Option<Claim> {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        match &mut *held {
            Some((by, n)) if *by == what => *n += 1,
            Some(_) => return None,
            None => *held = Some((what, 1)),
        }
        Some(Claim { radio: self.clone() })
    }

    pub fn in_use(&self, what: Use) -> bool {
        matches!(*self.held.lock().unwrap_or_else(|e| e.into_inner()), Some((by, _)) if by == what)
    }
}

/// Given back when dropped.
pub struct Claim {
    radio: Arc<Radio>,
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut held = self.radio.held.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, n)) = &mut *held {
            *n -= 1;
            if *n == 0 {
                *held = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_exclude_each_other() {
        let radio = Arc::new(Radio::default());
        let a = radio.try_claim(Use::AppleLink).unwrap();
        let b = radio.try_claim(Use::AppleLink).unwrap();
        assert!(radio.try_claim(Use::WifiSwitch).is_none());
        assert!(radio.in_use(Use::AppleLink));
        drop(a);
        assert!(radio.try_claim(Use::WifiSwitch).is_none());
        drop(b);
        assert!(!radio.in_use(Use::AppleLink));
        let _w = radio.try_claim(Use::WifiSwitch).unwrap();
        assert!(radio.try_claim(Use::AppleLink).is_none());
    }
}
