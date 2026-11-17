use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A reviewable translation table, bound to the exact UTF-8 HTML source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranslationTable {
    pub version: u32,
    pub source_sha256: String,
    pub source_language: Option<String>,
    pub target_language: Option<String>,
    pub entries: BTreeMap<String, TranslationEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Text,
    Attribute,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranslationEntry {
    pub kind: EntryKind,
    pub source: String,
    pub translation: Option<String>,
    pub context: String,
    /// Byte range in the original UTF-8 HTML.
    pub start: usize,
    pub end: usize,
    /// Reserved marker -> verbatim source markup or protected variable.
    pub placeholders: BTreeMap<String, String>,
}
