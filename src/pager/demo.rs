//! Display-only anonymization for demos.
//!
//! Demo mode must not touch session identity, resume commands, cache keys, or
//! project paths. It only changes what the pager paints: free text becomes
//! enjine-style alien glyphs, and repo labels not present in the public orgmap
//! are replaced with stable alien words.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use toml::Value;

const ENJINE_SCRAMBLE_CHARS: &str =
    "▓▒░█▄▀▌▐│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌αβγδεζηθικλμνξοπρστυφχψω∞∂∇∈∉∋∌∑∏√∛∜≈≠≤≥⊕⊗⊙⊘";

#[derive(Debug, Clone)]
pub struct DemoMode {
    public_projects: Arc<HashSet<String>>,
    symbols: Arc<Vec<char>>,
}

impl DemoMode {
    pub fn load_default() -> Self {
        let public_projects = load_public_orgmap_projects().unwrap_or_default();
        let symbols = ENJINE_SCRAMBLE_CHARS.chars().collect::<Vec<_>>();
        Self {
            public_projects: Arc::new(public_projects),
            symbols: Arc::new(symbols),
        }
    }

    pub fn anonymize_sessions(&self, sessions: &mut [super::session_list::EnrichedSession]) {
        for session in sessions {
            scramble_option(self, &mut session.display_name);
            scramble_option(self, &mut session.generated_title);
            scramble_option(self, &mut session.last_prompt);
        }
    }

    pub fn scramble_text(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        for (idx, ch) in text.chars().enumerate() {
            if ch.is_whitespace() {
                out.push(ch);
            } else {
                out.push(self.symbol_for(hash_char(ch, idx)));
            }
        }
        out
    }

    pub fn repo_label_for_path(&self, path: &Path) -> Option<String> {
        let leaf = project_leaf(path)?;
        if self.public_projects.contains(leaf) {
            None
        } else {
            Some(self.alien_word(&path.to_string_lossy(), leaf.chars().count()))
        }
    }

    fn alien_word(&self, seed: &str, source_len: usize) -> String {
        let len = source_len.clamp(4, 14);
        let mut out = String::with_capacity(len * 3);
        let seed_hash = fnv1a(seed.as_bytes());
        for idx in 0..len {
            out.push(
                self.symbol_for(seed_hash.wrapping_add((idx as u64 + 1).wrapping_mul(0x9e37_79b9))),
            );
        }
        out
    }

    fn symbol_for(&self, hash: u64) -> char {
        let symbols = self.symbols.as_slice();
        symbols[(hash as usize) % symbols.len()]
    }
}

fn scramble_option(demo: &DemoMode, value: &mut Option<String>) {
    if let Some(text) = value {
        *text = demo.scramble_text(text);
    }
}

fn project_leaf(path: &Path) -> Option<&str> {
    path.file_name().and_then(|name| name.to_str())
}

fn load_public_orgmap_projects() -> anyhow::Result<HashSet<String>> {
    let path = orgmap_path()?;
    let text = std::fs::read_to_string(path)?;
    let value = text.parse::<Value>()?;
    let mut names = HashSet::new();

    if let Some(sections) = value.get("sections").and_then(Value::as_table) {
        for entries in sections.values().filter_map(Value::as_array) {
            for entry in entries.iter().filter_map(Value::as_str) {
                if let Some(name) = orgmap_entry_project_name(entry) {
                    names.insert(name.to_string());
                }
            }
        }
    }

    if let Some(overrides) = value.get("overrides").and_then(Value::as_table) {
        names.extend(overrides.keys().cloned());
    }

    Ok(names)
}

fn orgmap_path() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("home directory not available"))?;
    Ok(home.join("holoq/orgmap.toml"))
}

fn orgmap_entry_project_name(entry: &str) -> Option<&str> {
    let name = entry
        .rsplit_once(':')
        .map(|(_, name)| name)
        .unwrap_or(entry);
    let name = name.trim();
    (!name.is_empty()).then_some(name)
}

fn hash_char(ch: char, idx: usize) -> u64 {
    let mut bytes = [0; 4];
    let text = ch.encode_utf8(&mut bytes);
    fnv1a(text.as_bytes()).wrapping_add((idx as u64 + 1).wrapping_mul(0x517c_c1b7_2722_0a95))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orgmap_entries_strip_stage_prefixes() {
        assert_eq!(orgmap_entry_project_name("3:babel"), Some("babel"));
        assert_eq!(orgmap_entry_project_name("archived:thaum"), Some("thaum"));
        assert_eq!(orgmap_entry_project_name("ripmap"), Some("ripmap"));
    }

    #[test]
    fn scramble_preserves_whitespace_shape() {
        let demo = DemoMode::load_default();
        let text = "hello world\nnext";
        let scrambled = demo.scramble_text(text);

        assert_eq!(scrambled.chars().filter(|ch| ch.is_whitespace()).count(), 2);
        assert_eq!(scrambled.chars().count(), text.chars().count());
        assert_ne!(scrambled, text);
    }
}
