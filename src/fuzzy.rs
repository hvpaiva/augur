//! Matching a mistyped word against the words it may have meant.
//!
//! A candidate matches when the typed word, ignoring case and the separators
//! `-`, `_` and `.`, is a prefix of it (`ble-o` for `bleopt`), or when one edit
//! turns the typed word into a prefix of it (`dokcer` for `docker`). An edit is
//! an insertion, a deletion, a substitution or a swap of adjacent characters.

/// Shortest typed word, in characters, that an edit may correct: below it too
/// many words are one edit away.
const MIN_EDITABLE: usize = 4;

/// Matches one typed word against many candidates.
pub struct Matcher {
    typed: Vec<char>,
    key: String,
}

impl Matcher {
    /// A matcher for `typed`; `None` when it holds nothing but separators.
    pub fn new(typed: &str) -> Option<Self> {
        let key = normalize(typed);
        (!key.is_empty()).then(|| Self {
            typed: typed.chars().collect(),
            key,
        })
    }

    /// How far the typed word is from the start of `candidate`: 0 when they
    /// differ only in case and separators, 1 when one edit apart, `None`
    /// otherwise.
    pub fn distance(&self, candidate: &str) -> Option<u8> {
        if normalize(candidate).starts_with(&self.key) {
            return Some(0);
        }
        (self.typed.len() >= MIN_EDITABLE && self.within_one_edit(candidate)).then_some(1)
    }

    /// Whether one edit turns the typed word into a prefix of `candidate`, by
    /// optimal string alignment. Such a prefix is at most one character longer
    /// than the typed word, so the rest of `candidate` is not looked at.
    fn within_one_edit(&self, candidate: &str) -> bool {
        let a = &self.typed;
        let b: Vec<char> = candidate.chars().take(a.len() + 1).collect();
        // Rows i - 2, i - 1 and i of the distances between a[..i] and b[..j].
        let mut before: Vec<usize> = vec![0; b.len() + 1];
        let mut previous: Vec<usize> = (0..=b.len()).collect();
        let mut current = vec![0; b.len() + 1];
        for i in 1..=a.len() {
            current[0] = i;
            for j in 1..=b.len() {
                let substitution = previous[j - 1] + usize::from(a[i - 1] != b[j - 1]);
                let mut best = substitution.min(previous[j] + 1).min(current[j - 1] + 1);
                if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                    best = best.min(before[j - 2] + 1);
                }
                current[j] = best;
            }
            std::mem::swap(&mut before, &mut previous);
            std::mem::swap(&mut previous, &mut current);
        }
        previous.iter().any(|&distance| distance <= 1)
    }
}

fn normalize(word: &str) -> String {
    word.chars()
        .filter(|c| !matches!(c, '-' | '_' | '.'))
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn distance(typed: &str, candidate: &str) -> Option<u8> {
        Matcher::new(typed)?.distance(candidate)
    }

    #[test]
    fn ignores_case_and_separators() {
        assert_eq!(distance("ble-o", "bleopt"), Some(0));
        assert_eq!(distance("Dock", "docker"), Some(0));
        assert_eq!(distance("docker_comp", "docker-compose"), Some(0));
    }

    #[test]
    fn allows_one_edit_towards_a_prefix() {
        assert_eq!(distance("ble-o", "ble-bind"), Some(1));
        assert_eq!(distance("dokcer", "docker"), Some(1));
        assert_eq!(distance("kubectk", "kubectl"), Some(1));
        assert_eq!(distance("dockr", "docker-compose"), Some(1));
        assert_eq!(distance("sytem", "systemctl"), Some(1));
    }

    #[test]
    fn rejects_distant_or_too_short_words() {
        assert_eq!(distance("xyz", "docker"), None);
        assert_eq!(distance("dcoekr", "docker"), None);
        assert_eq!(distance("ab", "ac"), None);
        assert_eq!(distance("gti", "git"), None);
        assert_eq!(distance("--", "docker"), None);
    }
}
