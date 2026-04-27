use anyhow::{Result, anyhow, bail};
use lixun_core::{PluginFieldSpec, PluginFieldType, TextTokenizer};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use tantivy::schema::{
    FAST, IndexRecordOption, NumericOptions, STORED, STRING, SchemaBuilder, TextFieldIndexing,
    TextOptions,
};

pub const FINGERPRINT_FILE: &str = "layout.fingerprint";

#[derive(Debug)]
pub struct CompiledField {
    pub field: tantivy::schema::Field,
    pub spec: PluginFieldSpec,
}

#[derive(Debug)]
pub struct CompiledPluginSchema {
    pub extras: HashMap<&'static str, CompiledField>,
    pub query_aliases: HashMap<&'static str, &'static str>,
    pub default_query_fields: Vec<(tantivy::schema::Field, f32)>,
}

impl CompiledPluginSchema {
    pub fn empty() -> Self {
        Self {
            extras: HashMap::new(),
            query_aliases: HashMap::new(),
            default_query_fields: Vec::new(),
        }
    }
}

pub fn add_plugin_fields_to_schema(
    builder: &mut SchemaBuilder,
    plugin_fields_by_kind: &BTreeMap<&'static str, &'static [PluginFieldSpec]>,
) -> Result<CompiledPluginSchema> {
    let mut seen_schema_names: HashMap<&'static str, &'static str> = HashMap::new();
    let mut seen_aliases: HashMap<&'static str, &'static str> = HashMap::new();

    let mut flat: Vec<(&'static str, PluginFieldSpec)> = Vec::new();
    for (kind, specs) in plugin_fields_by_kind {
        for spec in specs.iter() {
            if let Some(prev) = seen_schema_names.insert(spec.schema_name, kind) {
                bail!(
                    "Plugin schema field name collision: '{}' declared by both '{}' and '{}'",
                    spec.schema_name,
                    prev,
                    kind
                );
            }
            if let Some(alias) = spec.query_alias
                && let Some(prev) = seen_aliases.insert(alias, kind)
            {
                bail!(
                    "Plugin query alias collision: '{}' declared by both '{}' and '{}'",
                    alias,
                    prev,
                    kind
                );
            }
            flat.push((kind, *spec));
        }
    }

    flat.sort_by_key(|(_kind, spec)| spec.schema_name);

    let mut extras: HashMap<&'static str, CompiledField> = HashMap::new();
    let mut query_aliases: HashMap<&'static str, &'static str> = HashMap::new();
    let mut default_query_fields: Vec<(tantivy::schema::Field, f32)> = Vec::new();

    for (_kind, spec) in flat {
        let field = match spec.ty {
            PluginFieldType::Text { tokenizer } => {
                let tok_name = match tokenizer {
                    TextTokenizer::Default => "default",
                    TextTokenizer::Raw => "raw",
                    TextTokenizer::Spotlight => "spotlight",
                };
                let indexing = TextFieldIndexing::default()
                    .set_tokenizer(tok_name)
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions);
                let mut opts = TextOptions::default().set_indexing_options(indexing);
                if spec.stored {
                    opts = opts.set_stored();
                }
                builder.add_text_field(spec.schema_name, opts)
            }
            PluginFieldType::Keyword => {
                let opts = if spec.stored { STRING | STORED } else { STRING };
                builder.add_text_field(spec.schema_name, opts)
            }
            PluginFieldType::I64 => {
                let opts = if spec.stored {
                    NumericOptions::default()
                        .set_indexed()
                        .set_stored()
                        .set_fast()
                } else {
                    NumericOptions::default().set_indexed().set_fast()
                };
                builder.add_i64_field(spec.schema_name, opts)
            }
            PluginFieldType::U64 => {
                let opts = if spec.stored {
                    NumericOptions::default()
                        .set_indexed()
                        .set_stored()
                        .set_fast()
                } else {
                    NumericOptions::default().set_indexed().set_fast()
                };
                builder.add_u64_field(spec.schema_name, opts)
            }
            PluginFieldType::Bool => {
                let _ = FAST;
                let opts = if spec.stored {
                    STORED.into()
                } else {
                    NumericOptions::default()
                };
                builder.add_bool_field(spec.schema_name, opts)
            }
        };

        if let Some(alias) = spec.query_alias {
            query_aliases.insert(alias, spec.schema_name);
        }
        if spec.default_search {
            default_query_fields.push((field, spec.boost));
        }

        extras.insert(spec.schema_name, CompiledField { field, spec });
    }

    Ok(CompiledPluginSchema {
        extras,
        query_aliases,
        default_query_fields,
    })
}

pub fn compute_fingerprint(
    index_version: u32,
    plugin_fields_by_kind: &BTreeMap<&'static str, &'static [PluginFieldSpec]>,
) -> String {
    let mut canonical = String::new();
    canonical.push_str(&format!("v={}\n", index_version));
    for (kind, specs) in plugin_fields_by_kind {
        canonical.push_str(&format!("kind={}\n", kind));
        let mut specs_vec: Vec<&PluginFieldSpec> = specs.iter().collect();
        specs_vec.sort_by_key(|s| s.schema_name);
        for s in specs_vec {
            canonical.push_str(&format!(
                "  {}|alias={:?}|ty={:?}|stored={}|default={}|boost={}\n",
                s.schema_name, s.query_alias, s.ty, s.stored, s.default_search, s.boost,
            ));
        }
    }
    simple_hash(&canonical)
}

fn simple_hash(input: &str) -> String {
    let mut h1: u64 = 0xcbf2_9ce4_8422_2325;
    let mut h2: u64 = 0x100000001b3;
    for b in input.bytes() {
        h1 ^= b as u64;
        h1 = h1.wrapping_mul(0x100000001b3);
        h2 = h2.wrapping_add(b as u64);
        h2 = h2.rotate_left(13).wrapping_mul(0xcbf29ce484222325);
    }
    format!("{:016x}{:016x}", h1, h2)
}

pub fn read_fingerprint(index_dir: &Path) -> Option<String> {
    std::fs::read_to_string(index_dir.join(FINGERPRINT_FILE))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn write_fingerprint(index_dir: &Path, fingerprint: &str) -> Result<()> {
    std::fs::write(index_dir.join(FINGERPRINT_FILE), fingerprint)
        .map_err(|e| anyhow!("write fingerprint: {}", e))
}

pub fn rewrite_query_aliases(text: &str, aliases: &HashMap<&'static str, &'static str>) -> String {
    if aliases.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let is_id_start = c.is_ascii_alphabetic() || c == b'_';
        let at_boundary = i == 0 || !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_';
        if is_id_start && at_boundary {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b':' {
                let ident = &text[start..i];
                if let Some(real) = aliases.get(ident) {
                    out.push_str(real);
                    out.push(':');
                    i += 1;
                    continue;
                }
            }
            out.push_str(&text[start..i]);
        } else {
            out.push(c as char);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOLDER: PluginFieldSpec = PluginFieldSpec {
        schema_name: "maildir_folder",
        query_alias: Some("folder"),
        ty: PluginFieldType::Keyword,
        stored: true,
        default_search: false,
        boost: 0.0,
    };

    const FLAGS: PluginFieldSpec = PluginFieldSpec {
        schema_name: "maildir_flags",
        query_alias: Some("flags"),
        ty: PluginFieldType::Keyword,
        stored: true,
        default_search: false,
        boost: 0.0,
    };

    const BODY_EXTRA: PluginFieldSpec = PluginFieldSpec {
        schema_name: "maildir_hint",
        query_alias: None,
        ty: PluginFieldType::Text {
            tokenizer: TextTokenizer::Spotlight,
        },
        stored: false,
        default_search: true,
        boost: 2.0,
    };

    #[test]
    fn test_compile_rejects_duplicate_schema_name() {
        const DUP: PluginFieldSpec = PluginFieldSpec {
            schema_name: "maildir_folder",
            query_alias: Some("dup"),
            ty: PluginFieldType::Keyword,
            stored: true,
            default_search: false,
            boost: 0.0,
        };
        let mut map: BTreeMap<&'static str, &'static [PluginFieldSpec]> = BTreeMap::new();
        let a: &'static [PluginFieldSpec] = &[FOLDER];
        let b: &'static [PluginFieldSpec] = &[DUP];
        map.insert("maildir", a);
        map.insert("other", b);
        let mut b = SchemaBuilder::new();
        let err = add_plugin_fields_to_schema(&mut b, &map).unwrap_err();
        assert!(
            err.to_string().contains("schema field name collision"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_compile_rejects_duplicate_alias() {
        const DUP_ALIAS: PluginFieldSpec = PluginFieldSpec {
            schema_name: "other_folder",
            query_alias: Some("folder"),
            ty: PluginFieldType::Keyword,
            stored: true,
            default_search: false,
            boost: 0.0,
        };
        let mut map: BTreeMap<&'static str, &'static [PluginFieldSpec]> = BTreeMap::new();
        let a: &'static [PluginFieldSpec] = &[FOLDER];
        let b: &'static [PluginFieldSpec] = &[DUP_ALIAS];
        map.insert("maildir", a);
        map.insert("other", b);
        let mut b = SchemaBuilder::new();
        let err = add_plugin_fields_to_schema(&mut b, &map).unwrap_err();
        assert!(
            err.to_string().contains("query alias collision"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_compile_default_query_fields_includes_only_default_search() {
        let mut map: BTreeMap<&'static str, &'static [PluginFieldSpec]> = BTreeMap::new();
        let specs: &'static [PluginFieldSpec] = &[FOLDER, FLAGS, BODY_EXTRA];
        map.insert("maildir", specs);
        let mut b = SchemaBuilder::new();
        let cs = add_plugin_fields_to_schema(&mut b, &map).unwrap();
        assert_eq!(cs.default_query_fields.len(), 1);
        assert!((cs.default_query_fields[0].1 - 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_fingerprint_changes_when_fields_change() {
        let mut a: BTreeMap<&'static str, &'static [PluginFieldSpec]> = BTreeMap::new();
        let sa: &'static [PluginFieldSpec] = &[FOLDER];
        a.insert("maildir", sa);

        let mut b: BTreeMap<&'static str, &'static [PluginFieldSpec]> = BTreeMap::new();
        let sb: &'static [PluginFieldSpec] = &[FOLDER, FLAGS];
        b.insert("maildir", sb);

        let fa = compute_fingerprint(5, &a);
        let fb = compute_fingerprint(5, &b);
        assert_ne!(fa, fb);
    }

    #[test]
    fn test_fingerprint_stable_under_identical_input() {
        let mut a: BTreeMap<&'static str, &'static [PluginFieldSpec]> = BTreeMap::new();
        let sa: &'static [PluginFieldSpec] = &[FOLDER, FLAGS];
        a.insert("maildir", sa);
        assert_eq!(compute_fingerprint(5, &a), compute_fingerprint(5, &a));
    }

    #[test]
    fn test_rewrite_aliases_folder_to_schema() {
        let mut aliases: HashMap<&'static str, &'static str> = HashMap::new();
        aliases.insert("folder", "maildir_folder");
        assert_eq!(
            rewrite_query_aliases("folder:Inbox", &aliases),
            "maildir_folder:Inbox"
        );
    }

    #[test]
    fn test_rewrite_aliases_preserves_non_alias() {
        let mut aliases: HashMap<&'static str, &'static str> = HashMap::new();
        aliases.insert("folder", "maildir_folder");
        assert_eq!(
            rewrite_query_aliases("hello world", &aliases),
            "hello world"
        );
        assert_eq!(
            rewrite_query_aliases("unknown:value", &aliases),
            "unknown:value"
        );
    }

    #[test]
    fn test_rewrite_aliases_respects_word_boundary() {
        let mut aliases: HashMap<&'static str, &'static str> = HashMap::new();
        aliases.insert("folder", "maildir_folder");
        assert_eq!(rewrite_query_aliases("myfolder:x", &aliases), "myfolder:x");
    }

    #[test]
    fn test_rewrite_aliases_multiple() {
        let mut aliases: HashMap<&'static str, &'static str> = HashMap::new();
        aliases.insert("folder", "maildir_folder");
        aliases.insert("flags", "maildir_flags");
        assert_eq!(
            rewrite_query_aliases("folder:INBOX AND flags:seen", &aliases),
            "maildir_folder:INBOX AND maildir_flags:seen"
        );
    }
}
