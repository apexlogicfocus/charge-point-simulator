/// A scrolling log of session events, newest entry last.
///
/// The buffer auto-follows the tail of the log by default. Scrolling up
/// pauses that follow behavior so the view holds still even as new entries
/// arrive underneath; scrolling back down to the bottom resumes it.
#[derive(Debug, Clone, Default)]
pub struct LogBuffer {
    entries: Vec<String>,
    scroll_offset: usize,
    filter: Option<String>,
}

impl LogBuffer {
    pub fn push(&mut self, entry: impl Into<String>) {
        let entry = entry.into();
        let matches_filter = self.matches_filter(&entry);
        self.entries.push(entry);
        if self.is_paused() && matches_filter {
            self.scroll_offset += 1;
        }
    }

    /// Whether the view is scrolled up and no longer following the live tail.
    pub fn is_paused(&self) -> bool {
        self.scroll_offset > 0
    }

    pub fn scroll_up(&mut self) {
        let max_offset = self.filtered().len().saturating_sub(1);
        if self.scroll_offset < max_offset {
            self.scroll_offset += 1;
        }
    }

    pub fn scroll_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }

    pub fn set_filter(&mut self, filter: impl Into<String>) {
        self.filter = Some(filter.into());
        self.scroll_offset = 0;
    }

    pub fn clear_filter(&mut self) {
        self.filter = None;
        self.scroll_offset = 0;
    }

    /// The `height` most recent lines that should be visible, oldest first,
    /// honoring the current filter and scroll position. `scroll_offset` picks
    /// which line anchors the bottom of the view; near the oldest entry this
    /// can yield fewer than `height` lines rather than padding with newer
    /// ones the scroll position has intentionally moved past.
    pub fn visible_lines(&self, height: usize) -> Vec<&str> {
        let filtered = self.filtered();
        if filtered.is_empty() || height == 0 {
            return Vec::new();
        }

        let last_index = filtered.len() - 1;
        let bottom_index = last_index.saturating_sub(self.scroll_offset);
        let start_index = bottom_index.saturating_sub(height - 1);
        filtered[start_index..=bottom_index].to_vec()
    }

    fn matches_filter(&self, entry: &str) -> bool {
        match &self.filter {
            Some(filter) => entry.to_lowercase().contains(&filter.to_lowercase()),
            None => true,
        }
    }

    fn filtered(&self) -> Vec<&str> {
        self.entries
            .iter()
            .map(String::as_str)
            .filter(|entry| self.matches_filter(entry))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_buffer_is_empty_and_not_paused() {
        let logs = LogBuffer::default();
        assert_eq!(logs.visible_lines(5), Vec::<&str>::new());
        assert!(!logs.is_paused());
    }

    #[test]
    fn visible_lines_shows_the_most_recent_entries_first_to_last() {
        let mut logs = LogBuffer::default();
        for line in ["one", "two", "three", "four"] {
            logs.push(line);
        }

        assert_eq!(logs.visible_lines(2), vec!["three", "four"]);
    }

    #[test]
    fn visible_lines_returns_everything_when_the_buffer_is_shorter_than_the_height() {
        let mut logs = LogBuffer::default();
        logs.push("only one");

        assert_eq!(logs.visible_lines(5), vec!["only one"]);
    }

    #[test]
    fn scrolling_up_pauses_and_holds_the_view_steady_as_new_entries_arrive() {
        let mut logs = LogBuffer::default();
        for line in ["one", "two", "three", "four"] {
            logs.push(line);
        }

        logs.scroll_up();
        assert!(logs.is_paused());
        let before: Vec<String> = logs.visible_lines(2).into_iter().map(String::from).collect();
        assert_eq!(before, vec!["two", "three"]);

        logs.push("five");
        let after: Vec<String> = logs.visible_lines(2).into_iter().map(String::from).collect();
        assert_eq!(before, after);
    }

    #[test]
    fn scroll_up_stops_once_the_oldest_entry_reaches_the_bottom_of_the_view() {
        let mut logs = LogBuffer::default();
        for line in ["one", "two"] {
            logs.push(line);
        }

        for _ in 0..10 {
            logs.scroll_up();
        }

        // scrolled all the way up: the oldest entry is now the bottommost
        // visible line, so nothing newer than it shows.
        assert_eq!(logs.visible_lines(5), vec!["one"]);
    }

    #[test]
    fn scrolling_back_down_to_the_bottom_resumes_following_the_tail() {
        let mut logs = LogBuffer::default();
        for line in ["one", "two", "three"] {
            logs.push(line);
        }

        logs.scroll_up();
        logs.scroll_up();
        assert!(logs.is_paused());

        logs.scroll_down();
        logs.scroll_down();
        assert!(!logs.is_paused());
        assert_eq!(logs.visible_lines(2), vec!["two", "three"]);
    }

    #[test]
    fn scroll_down_at_the_bottom_does_not_panic() {
        let mut logs = LogBuffer::default();
        logs.push("one");
        logs.scroll_down();
        assert!(!logs.is_paused());
    }

    #[test]
    fn filter_hides_non_matching_entries() {
        let mut logs = LogBuffer::default();
        logs.push("connector 1 available");
        logs.push("connector 2 faulted");
        logs.push("heartbeat sent");

        logs.set_filter("fault");
        assert_eq!(logs.visible_lines(10), vec!["connector 2 faulted"]);
    }

    #[test]
    fn filter_matching_is_case_insensitive() {
        let mut logs = LogBuffer::default();
        logs.push("Connector Faulted");

        logs.set_filter("FAULT");
        assert_eq!(logs.visible_lines(10), vec!["Connector Faulted"]);
    }

    #[test]
    fn clearing_the_filter_restores_every_entry() {
        let mut logs = LogBuffer::default();
        logs.push("one");
        logs.push("two");

        logs.set_filter("one");
        logs.clear_filter();

        assert_eq!(logs.visible_lines(10), vec!["one", "two"]);
    }

    #[test]
    fn pushing_a_non_matching_entry_while_paused_does_not_shift_the_view() {
        let mut logs = LogBuffer::default();
        logs.push("connector faulted");
        logs.push("heartbeat sent");
        logs.set_filter("fault");

        logs.push("connector faulted again");
        assert!(!logs.is_paused());

        logs.scroll_up();
        let before: Vec<String> = logs.visible_lines(1).into_iter().map(String::from).collect();
        logs.push("heartbeat sent again");
        let after: Vec<String> = logs.visible_lines(1).into_iter().map(String::from).collect();
        assert_eq!(before, after);
    }
}
