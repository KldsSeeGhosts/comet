//! Primary-seat selection. Names are compatibility hints, not authorization.
use std::collections::BTreeMap;

pub(crate) fn synthetic(name: &str) -> bool {
    ["Cua-Agent", "Cua-Test-Agent"].iter().any(|prefix| {
        name == *prefix || name.strip_prefix(prefix).is_some_and(|suffix| suffix.starts_with('-'))
    })
}

pub(crate) struct Entry<T> {
    pub(crate) seat: T,
    pub(crate) name: Option<String>,
    pub(crate) capabilities: u32,
}

pub(crate) struct Seats<T> {
    entries: BTreeMap<u32, Entry<T>>,
    selected: Option<u32>,
}

impl<T: Clone + PartialEq> Seats<T> {
    pub(crate) fn new() -> Self { Self { entries: BTreeMap::new(), selected: None } }
    pub(crate) fn insert(&mut self, id: u32, seat: T, name: Option<String>) {
        self.entries.entry(id).or_insert(Entry { seat, name, capabilities: 0 });
        self.select();
    }
    fn eligible(entry: &Entry<T>) -> bool {
        entry.name.as_deref().is_some_and(|name| !name.is_empty() && !synthetic(name))
    }
    fn select(&mut self) {
        if self.selected.is_some_and(|id| self.entries.get(&id).is_some_and(Self::eligible)) {
            return;
        }
        self.selected = self.entries.iter().find(|(_, entry)| Self::eligible(entry)).map(|(id, _)| *id);
    }
    pub(crate) fn name(&mut self, seat: &T, name: String) {
        if let Some(entry) = self.entries.values_mut().find(|entry| &entry.seat == seat) {
            entry.name = Some(name);
        }
        self.select();
    }
    pub(crate) fn capabilities(&mut self, seat: &T, capabilities: u32) {
        if let Some(entry) = self.entries.values_mut().find(|entry| &entry.seat == seat) {
            entry.capabilities = capabilities;
        }
    }
    pub(crate) fn current(&self) -> Option<(T, u32)> {
        self.entries.get(&self.selected?).map(|entry| (entry.seat.clone(), entry.capabilities))
    }
    pub(crate) fn agents(&self) -> Vec<(u32, T, u32)> {
        self.entries.iter().filter(|(_, entry)| entry.name.as_deref().is_some_and(synthetic))
            .map(|(id, entry)| (*id, entry.seat.clone(), entry.capabilities)).collect()
    }
    pub(crate) fn contains(&self, seat: &T) -> bool {
        self.entries.values().any(|entry| &entry.seat == seat)
    }
    pub(crate) fn remove(&mut self, id: u32) -> Option<T> {
        let entry = self.entries.remove(&id)?;
        self.select();
        Some(entry.seat)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn agent_names_are_excluded_including_future_lanes() {
        for name in ["Cua-Agent", "Cua-Agent-2", "Cua-Agent-12", "Cua-Test-Agent", "Cua-Test-Agent-3"] {
            assert!(synthetic(name));
        }
        for name in ["seat0", "seat1", "Cua-Agency", "Cua-AgentX"] { assert!(!synthetic(name)); }
    }
    #[test]
    fn unknown_name_cannot_take_input_before_name_arrives() {
        let mut seats = Seats::new();
        seats.insert(5, 5, None);
        seats.capabilities(&5, 3);
        assert_eq!(seats.current(), None);
        seats.name(&5, "Cua-Agent".into());
        assert_eq!(seats.current(), None);
    }
    #[test]
    fn startup_order_does_not_select_agent() {
        for order in [[1, 2, 3], [3, 2, 1], [2, 1, 3]] {
            let mut seats = Seats::new();
            for id in order { seats.insert(id, id, None); }
            for id in order { seats.capabilities(&id, 3); }
            for id in order { seats.name(&id, if id == 1 { "seat0" } else { "Cua-Agent" }.into()); }
            assert_eq!(seats.current(), Some((1, 3)));
        }
    }
    #[test]
    fn ordinary_hotplug_does_not_replace_selected_seat() {
        let mut seats = Seats::new();
        seats.insert(20, 20, Some("seat0".into()));
        seats.capabilities(&20, 3);
        seats.insert(1, 1, Some("seat1".into()));
        assert_eq!(seats.current(), Some((20, 3)));
    }
    #[test]
    fn capability_changes_are_scoped_to_identity() {
        let mut seats = Seats::new();
        seats.insert(1, 1, Some("seat0".into()));
        seats.insert(2, 2, Some("Cua-Agent".into()));
        seats.capabilities(&1, 3);
        seats.capabilities(&2, 0);
        assert_eq!(seats.current(), Some((1, 3)));
        seats.capabilities(&1, 0);
        assert_eq!(seats.current(), Some((1, 0)));
    }
    #[test]
    fn removal_selects_only_named_ordinary_fallback() {
        let mut seats = Seats::new();
        seats.insert(1, 1, Some("seat0".into()));
        seats.insert(2, 2, Some("Cua-Agent".into()));
        seats.insert(3, 3, None);
        assert_eq!(seats.remove(1), Some(1));
        assert_eq!(seats.current(), None);
        seats.name(&3, "seat1".into());
        assert_eq!(seats.current(), Some((3, 0)));
    }
    #[test]
    fn reused_registry_id_has_no_inherited_name_or_capabilities() {
        let mut seats = Seats::new();
        seats.insert(1, 100, Some("seat0".into()));
        seats.capabilities(&100, 3);
        seats.remove(1);
        seats.insert(1, 200, None);
        seats.name(&100, "seat0".into());
        seats.capabilities(&100, 3);
        assert_eq!(seats.current(), None);
        seats.name(&200, "Cua-Agent".into());
        assert_eq!(seats.current(), None);
    }
    #[test]
    fn removing_agent_does_not_change_physical_capabilities() {
        let mut seats = Seats::new();
        seats.insert(1, 1, Some("seat0".into()));
        seats.capabilities(&1, 3);
        seats.insert(2, 2, Some("Cua-Agent".into()));
        seats.remove(2);
        assert_eq!(seats.current(), Some((1, 3)));
    }
}
