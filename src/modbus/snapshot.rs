use super::Space;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::time::Instant;

/// The words last read from the battery. Registers that fail to read keep their old words.
#[derive(Default)]
pub struct Snapshot {
    words: [BTreeMap<u16, u16>; 2],
    available: bool,
    last_success: Option<Instant>,
}

impl Snapshot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, space: Space, address: u16) -> Option<u16> {
        self.words[space as usize].get(&address).copied()
    }

    /// The words of the registers, if all of them have been read.
    pub fn words(&self, space: Space, start: u16, count: u16) -> Option<Vec<u16>> {
        (0..count).map(|i| self.get(space, start.checked_add(i)?)).collect()
    }

    /// Some read has succeeded, and the last one that did was not too long ago.
    pub fn available(&self) -> bool {
        self.available
    }

    /// When the battery stops being available unless another read succeeds; `None` if it isn't.
    pub(crate) fn expires_at(&self, after: Duration) -> Option<Instant> {
        self.available.then_some(self.last_success? + after)
    }

    /// Returns `true` if the stored word is new or has changed.
    pub(crate) fn set(&mut self, space: Space, address: u16, word: u16) -> bool {
        self.words[space as usize].insert(address, word) != Some(word)
    }

    /// Returns `true` if the battery has become available.
    pub(crate) fn record_success(&mut self, at: Instant) -> bool {
        self.last_success = Some(at);
        self.set_available(true)
    }

    /// Returns `true` if this is news.
    pub(crate) fn set_available(&mut self, available: bool) -> bool {
        std::mem::replace(&mut self.available, available) != available
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_are_per_space_and_only_complete_if_all_present() {
        let mut s = Snapshot::new();
        assert!(s.set(Space::Input, 10, 1));
        assert!(s.set(Space::Input, 11, 2));
        assert!(s.set(Space::Input, 13, 4));
        assert!(s.set(Space::Holding, 10, 5));
        assert!(!s.set(Space::Holding, 10, 5));
        assert_eq!(s.words(Space::Input, 10, 2), Some(vec![1, 2]));
        assert_eq!(s.words(Space::Input, 10, 4), None);
        assert_eq!(s.words(Space::Holding, 11, 1), None);
        assert_eq!(s.words(Space::Input, 65535, 2), None);
    }

    #[test]
    fn availability_follows_successes() {
        let mut s = Snapshot::new();
        let (after, now) = (Duration::from_secs(60), Instant::now());
        assert!(!s.available() && s.expires_at(after).is_none());
        assert!(s.record_success(now));
        assert!(!s.record_success(now));
        assert_eq!(s.expires_at(after), Some(now + after));
        assert!(s.set_available(false));
        assert_eq!(s.expires_at(after), None);
    }
}
