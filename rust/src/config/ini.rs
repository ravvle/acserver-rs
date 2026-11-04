//! A small INI reader with vanilla acServer's rules (its ks/ini parser):
//!
//! - `[NAME]` starts a section; keys before the first section are ignored.
//! - `KEY=VALUE` splits at the first `=`; both sides are trimmed.
//! - A value ends at `//`, so `NAME=Foo // note` is `Foo` (and a URL loses
//!   everything after `http:`, as in vanilla). `;` and `#` inside a value
//!   are kept.
//! - Lines starting with `;` or `#`, blank lines and lines without `=` are
//!   skipped.
//! - Names are case-sensitive. A repeated key overwrites the earlier one,
//!   and a repeated section adds to the first.

/// One parsed INI file: its sections in order of first appearance.
#[derive(Debug, Default)]
pub struct IniFile {
    sections: Vec<IniSection>,
}

/// One `[NAME]` section and its keys in file order.
#[derive(Debug, Default)]
pub struct IniSection {
    name: String,
    keys: Vec<(String, String)>,
}

impl IniSection {
    /// The section name, without brackets.
    pub fn name(&self) -> &str {
        &self.name
    }

    fn set(&mut self, key: &str, value: &str) {
        match self.keys.iter_mut().find(|(name, _)| name == key) {
            Some(entry) => entry.1 = value.to_string(),
            None => self.keys.push((key.to_string(), value.to_string())),
        }
    }
}

impl IniFile {
    /// Parses INI text. A leading byte-order mark is skipped.
    pub fn parse(text: &str) -> IniFile {
        let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
        let mut file = IniFile::default();
        // The index of the current section, `None` before the first one.
        let mut current: Option<usize> = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
                continue;
            }
            if line.starts_with('[') {
                let name = line.trim_matches(|c| c == '[' || c == ']').trim();
                current = Some(file.section_index(name));
                continue;
            }
            let (Some(section), Some((key, value))) = (current, line.split_once('=')) else {
                continue;
            };
            let value = match value.find("//") {
                Some(end) => &value[..end],
                None => value,
            };
            file.sections[section].set(key.trim(), value.trim());
        }
        file
    }

    /// The index of the named section, adding it if new.
    fn section_index(&mut self, name: &str) -> usize {
        if let Some(index) = self.sections.iter().position(|s| s.name == name) {
            return index;
        }
        self.sections.push(IniSection {
            name: name.to_string(),
            keys: Vec::new(),
        });
        self.sections.len() - 1
    }

    /// Every section in order of first appearance.
    pub fn sections(&self) -> &[IniSection] {
        &self.sections
    }

    /// Whether a section with exactly this name exists.
    pub fn has_section(&self, name: &str) -> bool {
        self.sections.iter().any(|s| s.name == name)
    }

    /// A key's value, or `None` when the section or key is missing.
    pub fn get(&self, section: &str, key: &str) -> Option<String> {
        let section = self.sections.iter().find(|s| s.name == section)?;
        section
            .keys
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    }

    /// Whether the key exists in the section.
    pub fn has_key(&self, section: &str, key: &str) -> bool {
        self.get(section, key).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_and_keys() {
        let f = IniFile::parse(
            "TOP=1\n[SERVER]\nNAME = Foo \r\nPORT=9600\n; comment\n# comment\nJUNK\n",
        );
        assert_eq!(f.get("SERVER", "NAME").as_deref(), Some("Foo"));
        assert_eq!(f.get("SERVER", "PORT").as_deref(), Some("9600"));
        assert!(
            !f.has_key("SERVER", "TOP"),
            "keys before a section are ignored"
        );
        assert!(!f.has_key("SERVER", "JUNK"));
        assert_eq!(f.sections().len(), 1);
    }

    #[test]
    fn slash_comments_cut_values_but_semicolons_stay() {
        let f =
            IniFile::parse("[S]\nA=Foo // note\nB=see http://example.com\nC=a ; b # c\nD=x=y\n");
        assert_eq!(f.get("S", "A").as_deref(), Some("Foo"));
        assert_eq!(f.get("S", "B").as_deref(), Some("see http:"));
        assert_eq!(f.get("S", "C").as_deref(), Some("a ; b # c"));
        assert_eq!(f.get("S", "D").as_deref(), Some("x=y"));
    }

    #[test]
    fn later_key_wins_and_sections_merge() {
        let f = IniFile::parse("[S]\nA=1\nB=2\n[T]\n[S]\nA=3\n");
        assert_eq!(f.get("S", "A").as_deref(), Some("3"));
        assert_eq!(f.get("S", "B").as_deref(), Some("2"));
        let names: Vec<&str> = f.sections().iter().map(|s| s.name()).collect();
        assert_eq!(names, ["S", "T"]);
    }

    #[test]
    fn names_are_trimmed_and_case_sensitive() {
        let f = IniFile::parse("\u{FEFF}[ SERVER ]\nname=x\n");
        assert!(f.has_section("SERVER"));
        assert!(!f.has_key("SERVER", "NAME"));
    }
}
