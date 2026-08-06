//! Fuzzy subsequence matching for the command palette. Replaces the plain
//! case-insensitive substring match on the command label with something that
//! tolerates a user typing e.g. `"pv"` or `"plgvhcl"` for "Plug in vehicle".

/// Score `needle` as a subsequence of `haystack`, case-insensitively.
/// `None` when `needle` is not a subsequence at all. Higher is better.
///
/// An empty `needle` matches everything with an equal score of `0`, so the
/// palette can show its full list unfiltered before the user types anything.
pub fn score(haystack: &str, needle: &str) -> Option<u32> {
    if needle.is_empty() {
        return Some(0);
    }

    let haystack_chars: Vec<char> = haystack.chars().flat_map(char::to_lowercase).collect();
    let needle_chars: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();

    let mut total: u32 = 0;
    let mut haystack_index = 0;
    let mut previous_match_index: Option<usize> = None;

    for &needle_char in &needle_chars {
        // The next occurrence at or after where the previous needle char matched, which is
        // what makes this a subsequence match rather than a set-membership test.
        let match_index = haystack_chars[haystack_index..]
            .iter()
            .position(|&c| c == needle_char)
            .map(|offset| haystack_index + offset)?;

        let is_contiguous = previous_match_index == Some(match_index.wrapping_sub(1));
        let is_word_start = match_index == 0 || !haystack_chars[match_index - 1].is_alphanumeric();

        let mut char_score = 1;
        if is_contiguous {
            char_score += 8;
        }
        if is_word_start {
            char_score += 4;
        }
        total += char_score;

        previous_match_index = Some(match_index);
        haystack_index = match_index + 1;
    }

    // An exact prefix match (needle matches the start of haystack character-for-character)
    // scores highest of all - reward it on top of the per-character scoring above.
    if haystack_chars.len() >= needle_chars.len()
        && haystack_chars[..needle_chars.len()] == needle_chars[..]
    {
        total += 100;
    }

    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_needle_matches_everything_with_an_equal_score() {
        assert_eq!(score("Plug in vehicle", ""), Some(0));
        assert_eq!(score("Present RFID card", ""), Some(0));
        assert_eq!(score("Plug in vehicle", ""), score("Present RFID card", ""));
    }

    #[test]
    fn matches_a_scattered_subsequence_case_insensitively() {
        assert!(score("Plug in vehicle", "pv").is_some());
        assert!(score("Plug in vehicle", "plgvhcl").is_some());
        assert!(score("Plug in vehicle", "PV").is_some());
    }

    #[test]
    fn non_subsequence_does_not_match() {
        assert_eq!(score("Plug in vehicle", "zzz"), None);
    }

    #[test]
    fn contiguous_runs_beat_scattered_matches() {
        let contiguous = score("Plug in vehicle", "plug").unwrap();
        let scattered = score("Plug in vehicle", "pgin").unwrap();
        assert!(
            contiguous > scattered,
            "{contiguous} should be > {scattered}"
        );
    }

    #[test]
    fn matches_at_the_start_of_a_word_beat_mid_word_matches() {
        // "v" at the start of "vehicle" should beat "e", a mid-word match in "vehicle".
        let word_start = score("Plug in vehicle", "v").unwrap();
        let mid_word = score("Plug in vehicle", "e").unwrap();
        assert!(word_start > mid_word, "{word_start} should be > {mid_word}");
    }

    #[test]
    fn exact_prefix_match_scores_highest_of_all() {
        let prefix = score("Plug in vehicle", "plug in vehicle").unwrap();
        let other = score("Plug in vehicle", "vehicle plug in").unwrap_or(0);
        let contiguous = score("Plug in vehicle", "plug").unwrap();
        assert!(prefix > other);
        assert!(prefix > contiguous);
    }

    #[test]
    fn is_unicode_safe_and_operates_on_chars_not_bytes() {
        // A multi-byte char must be matched as one unit, and must not throw off the word-start
        // and contiguity checks that index into the haystack - a byte-indexed implementation
        // either panics here or mis-scores what follows the accented char.
        assert!(score("Café menu", "café").is_some());
        assert!(score("Café menu", "ém").is_some());
        assert!(score("Café menu", "x").is_none());

        // "caf" is contiguous from the start; "cfé" skips a char, so it must score lower.
        assert!(score("Café menu", "caf").unwrap() > score("Café menu", "cfé").unwrap());
    }
}
