//! Lixun Index — Tantivy wrapper: schema, writer, searcher.

pub mod normalize;
pub mod plugin_schema;
pub mod scoring;
pub mod tokenizer;

use anyhow::Result;
use chrono::Utc;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use tantivy::{
    Index, IndexWriter, TantivyDocument, Term,
    collector::TopDocs,
    directory::MmapDirectory,
    doc,
    query::{BooleanQuery, QueryParser, TermQuery},
    schema::{
        IndexRecordOption, STORED, STRING, Schema, TEXT, TextFieldIndexing, TextOptions, Value,
    },
};

pub use plugin_schema::CompiledPluginSchema;
pub use tantivy::{IndexWriter as TantivyIndexWriter, TantivyDocument as TantivyDoc};

use lixun_core::{
    Category, DocId, Document, ExtraFieldValue, Hit, PluginFieldSpec, PluginFieldType, PluginValue,
    Query, RankingConfig,
};
use normalize::normalize_for_match;

const INDEX_VERSION: u32 = 8;
const INDEX_VERSION_FILE: &str = "index_version.txt";

/// Tantivy schema fields.
pub struct LixunSchema {
    pub schema: Schema,
    pub id: tantivy::schema::Field,
    pub category: tantivy::schema::Field,
    pub title: tantivy::schema::Field,
    pub title_terms: tantivy::schema::Field,
    pub title_initials: tantivy::schema::Field,
    pub title_prefixes: tantivy::schema::Field,
    pub subtitle: tantivy::schema::Field,
    pub icon_name: tantivy::schema::Field,
    pub kind_label: tantivy::schema::Field,
    pub body: tantivy::schema::Field,
    pub path: tantivy::schema::Field,
    pub mtime: tantivy::schema::Field,
    pub size: tantivy::schema::Field,
    pub action: tantivy::schema::Field,
    pub secondary_action: tantivy::schema::Field,
    pub extract_fail: tantivy::schema::Field,
    pub sender: tantivy::schema::Field,
    pub recipients: tantivy::schema::Field,
    pub source_instance: tantivy::schema::Field,
}

impl LixunSchema {
    pub fn build() -> Self {
        let (s, _) = Self::build_with_plugins(&BTreeMap::new()).expect("empty plugin map");
        s
    }

    pub fn build_with_plugins(
        plugin_fields_by_kind: &BTreeMap<&'static str, &'static [PluginFieldSpec]>,
    ) -> Result<(Self, CompiledPluginSchema)> {
        let mut builder = tantivy::schema::Schema::builder();
        let spotlight_indexing = || {
            TextFieldIndexing::default()
                .set_tokenizer("spotlight")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions)
        };
        let stored_spotlight_text = || {
            TextOptions::default()
                .set_indexing_options(spotlight_indexing())
                .set_stored()
        };
        let indexed_spotlight_text =
            || TextOptions::default().set_indexing_options(spotlight_indexing());

        let id = builder.add_text_field("id", STRING | STORED);
        let category = builder.add_text_field("category", TEXT | STORED);
        let title = builder.add_text_field("title", stored_spotlight_text());
        let title_terms = builder.add_text_field("title_terms", indexed_spotlight_text());
        let title_initials = builder.add_text_field("title_initials", indexed_spotlight_text());
        let title_prefixes = builder.add_text_field("title_prefixes", indexed_spotlight_text());
        let subtitle = builder.add_text_field("subtitle", STORED);
        let icon_name = builder.add_text_field("icon_name", STORED);
        let kind_label = builder.add_text_field("kind_label", STORED);
        let body = builder.add_text_field("body", stored_spotlight_text());
        let path = builder.add_text_field("path", stored_spotlight_text());
        let mtime = builder.add_i64_field("mtime", STORED);
        let size = builder.add_u64_field("size", STORED);
        let action = builder.add_text_field("action", STORED);
        let secondary_action = builder.add_text_field("secondary_action", STORED);
        let extract_fail = builder.add_bool_field("extract_fail", STORED);
        let sender = builder.add_text_field("sender", stored_spotlight_text());
        let recipients = builder.add_text_field("recipients", stored_spotlight_text());
        let source_instance = builder.add_text_field("source_instance", STRING | STORED);

        let plugins =
            plugin_schema::add_plugin_fields_to_schema(&mut builder, plugin_fields_by_kind)?;

        let schema = builder.build();

        Ok((
            Self {
                schema,
                id,
                category,
                title,
                title_terms,
                title_initials,
                title_prefixes,
                subtitle,
                icon_name,
                kind_label,
                body,
                path,
                mtime,
                size,
                action,
                secondary_action,
                extract_fail,
                sender,
                recipients,
                source_instance,
            },
            plugins,
        ))
    }
}

/// Index wrapper with search and upsert.
pub struct LixunIndex {
    index: Index,
    schema: LixunSchema,
    plugins: CompiledPluginSchema,
    ranking: RankingConfig,
}

impl LixunIndex {
    pub fn create_or_open(index_path: &str, ranking: RankingConfig) -> Result<Self> {
        Self::create_or_open_with_plugins(index_path, &BTreeMap::new(), ranking)
            .map(|(index, _)| index)
    }

    /// Open or create the index.
    ///
    /// Returns `(index, rebuilt_from_scratch)`. `rebuilt_from_scratch` is
    /// `true` when the on-disk directory was missing or wiped (INDEX_VERSION
    /// change, schema fingerprint change, or missing meta.json). The daemon
    /// uses this flag to decide whether to re-emit every non-fs source's
    /// `reindex_full` on startup; when `false`, sources catch up via
    /// `on_fs_events` / `on_tick` instead.
    pub fn create_or_open_with_plugins(
        index_path: &str,
        plugin_fields_by_kind: &BTreeMap<&'static str, &'static [PluginFieldSpec]>,
        ranking: RankingConfig,
    ) -> Result<(Self, bool)> {
        let (schema, plugins) = LixunSchema::build_with_plugins(plugin_fields_by_kind)?;
        let index_dir = PathBuf::from(index_path);
        let version_path = index_dir.join(INDEX_VERSION_FILE);
        let meta_path = index_dir.join("meta.json");

        let fingerprint = plugin_schema::compute_fingerprint(INDEX_VERSION, plugin_fields_by_kind);
        let on_disk_fp = plugin_schema::read_fingerprint(&index_dir);
        let version_matches = read_index_version(&version_path) == Some(INDEX_VERSION);
        let fingerprint_matches = on_disk_fp.as_deref() == Some(fingerprint.as_str());
        let has_meta = meta_path.exists();

        let needs_rebuild = !version_matches || !fingerprint_matches || !has_meta;

        let index = if !needs_rebuild {
            Index::open_in_dir(index_path)?
        } else {
            if index_dir.exists() && !fingerprint_matches && version_matches {
                tracing::info!(
                    path = %index_dir.display(),
                    "Schema fingerprint changed; wiping and rebuilding index"
                );
            }
            recreate_index_dir(&index_dir, read_index_version_string(&version_path))?;
            let dir = MmapDirectory::open(index_path)?;
            let index = Index::create(
                dir,
                schema.schema.clone(),
                tantivy::IndexSettings::default(),
            )?;
            std::fs::write(&version_path, INDEX_VERSION.to_string())?;
            plugin_schema::write_fingerprint(&index_dir, &fingerprint)?;
            index
        };

        tokenizer::register_spotlight_tokenizer(&index);

        Ok((
            Self {
                index,
                schema,
                plugins,
                ranking,
            },
            needs_rebuild,
        ))
    }

    /// Upsert a document (delete by id, then insert).
    pub fn upsert(
        &mut self,
        doc: &Document,
        writer: &mut IndexWriter<TantivyDocument>,
    ) -> Result<()> {
        let s = &self.schema;

        let term = tantivy::Term::from_field_text(s.id, &doc.id.0);
        writer.delete_term(term);

        let action_json = serde_json::to_string(&doc.action)?;
        let secondary_action_json = doc
            .secondary_action
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?
            .unwrap_or_default();
        let title_split = tokenizer::split_identifiers(&doc.title);
        let title_initials_indexed = scoring::acronym_initials_indexed(&doc.title);
        let title_prefixes_indexed = scoring::compute_title_prefixes(&doc.title);

        let mut tdoc = doc![
            s.id => doc.id.0.as_str(),
            s.category => doc.category.as_str(),
            s.title => doc.title.as_str(),
            s.title_terms => title_split.as_str(),
            s.title_initials => title_initials_indexed.as_str(),
            s.title_prefixes => title_prefixes_indexed.as_str(),
            s.subtitle => doc.subtitle.as_str(),
            s.icon_name => doc.icon_name.as_deref().unwrap_or(""),
            s.kind_label => doc.kind_label.as_deref().unwrap_or(""),
            s.body => doc.body.as_deref().unwrap_or(""),
            s.path => doc.path.as_str(),
            s.mtime => doc.mtime,
            s.size => doc.size,
            s.action => action_json.as_str(),
            s.secondary_action => secondary_action_json.as_str(),
            s.extract_fail => doc.extract_fail,
            s.sender => doc.sender.as_deref().unwrap_or(""),
            s.recipients => doc.recipients.as_deref().unwrap_or(""),
            s.source_instance => doc.source_instance.as_str(),
        ];

        for extra in &doc.extra {
            let Some(cf) = self.plugins.extras.get(extra.field) else {
                anyhow::bail!(
                    "Document carries extra field '{}' not registered by any enabled plugin",
                    extra.field
                );
            };
            match (&cf.spec.ty, &extra.value) {
                (PluginFieldType::Text { .. }, PluginValue::Text(v))
                | (PluginFieldType::Keyword, PluginValue::Text(v)) => {
                    tdoc.add_text(cf.field, v);
                }
                (PluginFieldType::I64, PluginValue::I64(v)) => tdoc.add_i64(cf.field, *v),
                (PluginFieldType::U64, PluginValue::U64(v)) => tdoc.add_u64(cf.field, *v),
                (PluginFieldType::Bool, PluginValue::Bool(v)) => tdoc.add_bool(cf.field, *v),
                _ => anyhow::bail!(
                    "Type mismatch for plugin field '{}': spec={:?}, value={:?}",
                    extra.field,
                    cf.spec.ty,
                    extra.value
                ),
            }
        }

        writer.add_document(tdoc)?;

        Ok(())
    }

    pub fn delete_by_source_instance(
        &mut self,
        instance_id: &str,
        writer: &mut IndexWriter<TantivyDocument>,
    ) -> Result<()> {
        let term = tantivy::Term::from_field_text(self.schema.source_instance, instance_id);
        writer.delete_term(term);
        Ok(())
    }

    /// Delete a document by id.
    pub fn delete_by_id(
        &mut self,
        id: &str,
        writer: &mut IndexWriter<TantivyDocument>,
    ) -> Result<()> {
        let term = tantivy::Term::from_field_text(self.schema.id, id);
        writer.delete_term(term);
        Ok(())
    }

    /// Search the index with fuzzy matching.
    pub fn search(&self, query: &Query) -> Result<Vec<Hit>> {
        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let s = &self.schema;
        let q_norm = normalize_for_match(&query.text);
        let now_secs = Utc::now().timestamp();

        let query_obj = build_search_query(&query.text, &self.index, s, &self.plugins);
        let top_docs = searcher.search(&query_obj, &TopDocs::with_limit(query.limit as usize))?;

        let mut results = Vec::new();
        for (score, doc_address) in top_docs {
            let doc: TantivyDocument = searcher.doc(doc_address)?;

            let id = doc
                .get_first(s.id)
                .and_then(|value| value.as_str())
                .unwrap_or("?")
                .to_string();

            let category = match doc
                .get_first(s.category)
                .and_then(|value| value.as_str())
                .unwrap_or("file")
            {
                "app" => Category::App,
                "file" => Category::File,
                "mail" => Category::Mail,
                "attachment" => Category::Attachment,
                _ => Category::File,
            };

            let title = doc
                .get_first(s.title)
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string();
            let mtime = doc
                .get_first(s.mtime)
                .and_then(|value| value.as_i64())
                .unwrap_or(0);
            let title_norm = normalize_for_match(&title);

            let subtitle = doc
                .get_first(s.subtitle)
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string();

            let icon_name = stored_optional_text(&doc, s.icon_name);
            let kind_label = stored_optional_text(&doc, s.kind_label);

            let action_json = doc
                .get_first(s.action)
                .and_then(|value| value.as_str())
                .unwrap_or("{}");

            let action: lixun_core::Action = serde_json::from_str(action_json)
                .unwrap_or(lixun_core::Action::OpenFile { path: "".into() });

            let secondary_action_raw = doc
                .get_first(s.secondary_action)
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let secondary_action = if secondary_action_raw.is_empty() {
                None
            } else {
                serde_json::from_str::<lixun_core::Action>(secondary_action_raw)
                    .ok()
                    .map(Box::new)
            };

            let extract_fail = doc
                .get_first(s.extract_fail)
                .and_then(|value| value.as_bool())
                .unwrap_or(false);

            let sender = stored_optional_text(&doc, s.sender);
            let recipients = stored_optional_text(&doc, s.recipients);
            let body = stored_optional_text(&doc, s.body);
            let doc_mult = self.ranking.multiplier_for(category)
                * scoring::prefix_mult(&title_norm, &q_norm, self.ranking.prefix_boost)
                * scoring::acronym_mult(&title, &q_norm, self.ranking.acronym_boost)
                * scoring::recency_mult(
                    category,
                    mtime,
                    now_secs,
                    self.ranking.recency_weight,
                    self.ranking.recency_tau_days,
                );

            results.push(Hit {
                id: lixun_core::DocId(id),
                category,
                title,
                subtitle,
                icon_name,
                kind_label,
                score: score * doc_mult,
                action,
                extract_fail,
                sender,
                recipients,
                body,
                secondary_action,
            });
        }

        Ok(results)
    }

    /// Commit pending writes.
    pub fn commit(&mut self, writer: &mut IndexWriter<TantivyDocument>) -> Result<()> {
        writer.commit()?;
        Ok(())
    }

    /// All `id` values in the live index. O(N) over stored docs; intended for
    /// cross-checking a manifest against the index, not the search hot path.
    pub fn all_doc_ids(&self) -> Result<HashSet<String>> {
        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let mut out: HashSet<String> = HashSet::new();
        for (segment_ord, segment_reader) in searcher.segment_readers().iter().enumerate() {
            let alive = segment_reader.alive_bitset();
            for doc_id in 0..segment_reader.max_doc() {
                if let Some(bitset) = alive
                    && !bitset.is_alive(doc_id)
                {
                    continue;
                }
                let addr = tantivy::DocAddress::new(segment_ord as u32, doc_id);
                let doc: TantivyDocument = searcher.doc(addr)?;
                if let Some(id) = doc.get_first(self.schema.id).and_then(|v| v.as_str()) {
                    out.insert(id.to_string());
                }
            }
        }
        Ok(out)
    }

    /// Fetch the full `Document` by its stable `id`. Returns `Ok(None)`
    /// when no live doc matches (deleted, or never indexed). Intended
    /// for the OCR worker's body-upsert path: read the existing doc,
    /// replace `body`, write it back via `upsert`.
    ///
    /// Only reads stored fields. Plugin `extra` fields are reconstructed
    /// from the stored subset; plugin fields declared with `stored:
    /// false` are dropped (they would be dropped on any re-upsert
    /// anyway, so this is consistent with `upsert`'s round-trip).
    pub fn get_doc_by_id(&self, id: &str) -> Result<Option<Document>> {
        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let term = Term::from_field_text(self.schema.id, id);
        let query = TermQuery::new(term, IndexRecordOption::Basic);
        let top_docs = searcher.search(&query, &TopDocs::with_limit(1))?;
        let Some((_, addr)) = top_docs.first() else {
            return Ok(None);
        };
        let tdoc: TantivyDocument = searcher.doc(*addr)?;
        Ok(Some(self.doc_from_tantivy(&tdoc)))
    }

    /// Fetch just the `body` field for the doc with the given `id`.
    /// Returns `Ok(Some(body))` only when a live doc matches AND its
    /// stored body is non-empty after trimming; otherwise `Ok(None)`.
    ///
    /// Used by the DB-16 OCR enqueue short-circuit: if a prior OCR
    /// pass already populated the body, re-crawling the document must
    /// not re-enqueue it. Cheaper than `get_doc_by_id` — only looks at
    /// one stored field instead of reconstructing the full `Document`.
    pub fn get_body_by_id(&self, id: &str) -> Result<Option<String>> {
        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let term = Term::from_field_text(self.schema.id, id);
        let query = TermQuery::new(term, IndexRecordOption::Basic);
        let top_docs = searcher.search(&query, &TopDocs::with_limit(1))?;
        let Some((_, addr)) = top_docs.first() else {
            return Ok(None);
        };
        let tdoc: TantivyDocument = searcher.doc(*addr)?;
        let body = stored_optional_text(&tdoc, self.schema.body);
        match body {
            Some(text) if !text.trim().is_empty() => Ok(Some(text)),
            _ => Ok(None),
        }
    }

    fn doc_from_tantivy(&self, tdoc: &TantivyDocument) -> Document {
        let s = &self.schema;
        let id = tdoc
            .get_first(s.id)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let category = match tdoc
            .get_first(s.category)
            .and_then(|v| v.as_str())
            .unwrap_or("file")
        {
            "app" => Category::App,
            "file" => Category::File,
            "mail" => Category::Mail,
            "attachment" => Category::Attachment,
            "calculator" => Category::Calculator,
            "shell" => Category::Shell,
            _ => Category::File,
        };
        let title = tdoc
            .get_first(s.title)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let subtitle = tdoc
            .get_first(s.subtitle)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let icon_name = stored_optional_text(tdoc, s.icon_name);
        let kind_label = stored_optional_text(tdoc, s.kind_label);
        let body = stored_optional_text(tdoc, s.body);
        let path = tdoc
            .get_first(s.path)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let mtime = tdoc.get_first(s.mtime).and_then(|v| v.as_i64()).unwrap_or(0);
        let size = tdoc.get_first(s.size).and_then(|v| v.as_u64()).unwrap_or(0);
        let action_json = tdoc
            .get_first(s.action)
            .and_then(|v| v.as_str())
            .unwrap_or("{}");
        let action: lixun_core::Action = serde_json::from_str(action_json)
            .unwrap_or(lixun_core::Action::OpenFile { path: "".into() });
        let secondary_action_raw = tdoc
            .get_first(s.secondary_action)
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let secondary_action = if secondary_action_raw.is_empty() {
            None
        } else {
            serde_json::from_str::<lixun_core::Action>(secondary_action_raw).ok()
        };
        let extract_fail = tdoc
            .get_first(s.extract_fail)
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let sender = stored_optional_text(tdoc, s.sender);
        let recipients = stored_optional_text(tdoc, s.recipients);
        let source_instance = tdoc
            .get_first(s.source_instance)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Non-stored plugin fields are never persisted, so a round-trip
        // through the index always drops them — same as a re-upsert.
        let mut extra: Vec<ExtraFieldValue> = Vec::new();
        for (name, cf) in &self.plugins.extras {
            if !cf.spec.stored {
                continue;
            }
            let value = match cf.spec.ty {
                PluginFieldType::Text { .. } | PluginFieldType::Keyword => tdoc
                    .get_first(cf.field)
                    .and_then(|v| v.as_str())
                    .map(|s| PluginValue::Text(s.to_string())),
                PluginFieldType::I64 => tdoc
                    .get_first(cf.field)
                    .and_then(|v| v.as_i64())
                    .map(PluginValue::I64),
                PluginFieldType::U64 => tdoc
                    .get_first(cf.field)
                    .and_then(|v| v.as_u64())
                    .map(PluginValue::U64),
                PluginFieldType::Bool => tdoc
                    .get_first(cf.field)
                    .and_then(|v| v.as_bool())
                    .map(PluginValue::Bool),
            };
            if let Some(value) = value {
                extra.push(ExtraFieldValue { field: name, value });
            }
        }

        Document {
            id: DocId(id),
            category,
            title,
            subtitle,
            icon_name,
            kind_label,
            body,
            path,
            mtime,
            size,
            action,
            extract_fail,
            sender,
            recipients,
            source_instance,
            extra,
            secondary_action,
        }
    }

    /// Create an index writer.
    pub fn writer(&self, heap_size: usize) -> Result<IndexWriter<TantivyDocument>> {
        let writer = self.index.writer(heap_size)?;
        Ok(writer)
    }
}

fn stored_optional_text(doc: &TantivyDocument, field: tantivy::schema::Field) -> Option<String> {
    let text = doc
        .get_first(field)
        .and_then(|value| value.as_str())
        .unwrap_or("");

    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

fn read_index_version(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_index_version_string(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|contents| contents.trim().to_string())
        .filter(|contents| !contents.is_empty())
}

fn recreate_index_dir(index_dir: &Path, old_version: Option<String>) -> Result<()> {
    if index_dir.exists() {
        tracing::info!(
            path = %index_dir.display(),
            old_version = old_version.as_deref().unwrap_or("missing"),
            new_version = INDEX_VERSION,
            "Wiping stale index directory"
        );
        std::fs::remove_dir_all(index_dir)?;
    }

    std::fs::create_dir_all(index_dir)?;
    Ok(())
}

fn build_search_query(
    text: &str,
    index: &tantivy::Index,
    s: &LixunSchema,
    plugins: &CompiledPluginSchema,
) -> Box<dyn tantivy::query::Query> {
    let text = text.trim();
    if text.is_empty() {
        return Box::new(BooleanQuery::new(vec![]));
    }

    let aliased = plugin_schema::rewrite_query_aliases(text, &plugins.query_aliases);
    let normalized_text = aliased.replace('|', " OR ");

    let mut default_fields: Vec<tantivy::schema::Field> = vec![
        s.title,
        s.title_terms,
        s.title_initials,
        s.title_prefixes,
        s.body,
        s.path,
        s.sender,
        s.recipients,
    ];
    for (field, _boost) in &plugins.default_query_fields {
        default_fields.push(*field);
    }

    let mut parser = QueryParser::for_index(index, default_fields);
    parser.set_conjunction_by_default();
    parser.set_field_boost(s.title, 5.0);
    parser.set_field_boost(s.title_terms, 4.0);
    parser.set_field_boost(s.title_initials, 3.0);
    parser.set_field_boost(s.title_prefixes, 2.5);
    parser.set_field_boost(s.sender, 3.0);
    parser.set_field_boost(s.recipients, 2.5);
    parser.set_field_boost(s.path, 1.5);
    parser.set_field_boost(s.body, 1.0);
    parser.set_field_fuzzy(s.title, false, 1, true);
    parser.set_field_fuzzy(s.title_terms, false, 1, true);
    parser.set_field_fuzzy(s.body, false, 1, true);
    parser.set_field_fuzzy(s.sender, false, 1, true);
    parser.set_field_fuzzy(s.recipients, false, 1, true);

    for (field, boost) in &plugins.default_query_fields {
        parser.set_field_boost(*field, *boost);
    }

    parser.parse_query_lenient(&normalized_text).0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_index_with_docs(docs: &[Document]) -> (tempfile::TempDir, LixunIndex) {
        create_index_with_docs_and_ranking(docs, RankingConfig::default())
    }

    fn create_index_with_docs_and_ranking(
        docs: &[Document],
        ranking: RankingConfig,
    ) -> (tempfile::TempDir, LixunIndex) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_str().unwrap();

        let mut index = LixunIndex::create_or_open(path, ranking).unwrap();
        let mut writer = index.writer(20_000_000).unwrap();

        for doc in docs {
            index.upsert(doc, &mut writer).unwrap();
        }

        index.commit(&mut writer).unwrap();
        (tmp, index)
    }

    fn search(index: &LixunIndex, text: &str) -> Vec<Hit> {
        index
            .search(&Query {
                text: text.to_string(),
                limit: 10,
            })
            .unwrap()
    }

    fn sample_document(id: &str, title: &str, body: &str) -> Document {
        Document {
            id: lixun_core::DocId(id.to_string()),
            category: Category::File,
            title: title.to_string(),
            subtitle: id.to_string(),
            icon_name: None,
            kind_label: None,
            body: Some(body.to_string()),
            path: id.trim_start_matches("fs:").to_string(),
            mtime: 0,
            size: 100,
            action: lixun_core::Action::OpenFile {
                path: id.trim_start_matches("fs:").into(),
            },
            extract_fail: false,
            sender: None,
            recipients: None,
            source_instance: "test".into(),
            secondary_action: None,
            extra: Vec::new(),
        }
    }

    #[test]
    fn test_create_and_search() {
        let doc = sample_document(
            "fs:/tmp/test.txt",
            "test.txt",
            "hello world this is test content",
        );

        let (_tmp, index) = create_index_with_docs(&[doc]);
        let results = search(&index, "hello");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "test.txt");
        assert_eq!(results[0].icon_name, None);
        assert_eq!(results[0].kind_label, None);
    }

    #[test]
    fn test_search_by_sender_and_recipients() {
        let mut doc = sample_document(
            "mail:1",
            "Weekly sync notes",
            "body text that must not match",
        );
        doc.category = Category::Mail;
        doc.sender = Some("alice@example.com".into());
        doc.recipients = Some("bob@example.com, carol@example.com".into());

        let (_tmp, index) = create_index_with_docs(&[doc]);

        let by_sender = search(&index, "alice@example.com");
        assert_eq!(
            by_sender.len(),
            1,
            "sender field must be searchable by email address"
        );
        assert_eq!(by_sender[0].title, "Weekly sync notes");

        let by_recipient = search(&index, "carol@example.com");
        assert_eq!(
            by_recipient.len(),
            1,
            "recipients field must be searchable by email address"
        );
        assert_eq!(by_recipient[0].title, "Weekly sync notes");
    }

    #[test]
    fn test_delete_by_id() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_str().unwrap();

        let mut index = LixunIndex::create_or_open(path, RankingConfig::default()).unwrap();
        let mut writer = index.writer(20_000_000).unwrap();
        let doc = sample_document(
            "fs:/tmp/delete_me.txt",
            "delete_me.txt",
            "this will be deleted",
        );

        index.upsert(&doc, &mut writer).unwrap();
        index.commit(&mut writer).unwrap();
        writer.wait_merging_threads().unwrap();

        let mut writer = index.writer(20_000_000).unwrap();
        index
            .delete_by_id("fs:/tmp/delete_me.txt", &mut writer)
            .unwrap();
        index
            .delete_by_id("fs:/nonexistent.txt", &mut writer)
            .unwrap();
        index.commit(&mut writer).unwrap();
        writer.wait_merging_threads().unwrap();

        let results = index
            .search(&Query {
                text: "deleted".to_string(),
                limit: 10,
            })
            .unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_all_doc_ids_lists_live_docs_and_excludes_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_str().unwrap();

        let mut index = LixunIndex::create_or_open(path, RankingConfig::default()).unwrap();
        let mut writer = index.writer(20_000_000).unwrap();
        for i in 0..3 {
            index
                .upsert(
                    &sample_document(&format!("fs:/tmp/a{i}.txt"), &format!("a{i}.txt"), "body"),
                    &mut writer,
                )
                .unwrap();
        }
        index.commit(&mut writer).unwrap();
        writer.wait_merging_threads().unwrap();

        let mut writer = index.writer(20_000_000).unwrap();
        let ids = index.all_doc_ids().unwrap();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains("fs:/tmp/a0.txt"));
        assert!(ids.contains("fs:/tmp/a1.txt"));
        assert!(ids.contains("fs:/tmp/a2.txt"));

        index.delete_by_id("fs:/tmp/a1.txt", &mut writer).unwrap();
        index.commit(&mut writer).unwrap();
        writer.wait_merging_threads().unwrap();

        let ids = index.all_doc_ids().unwrap();
        assert_eq!(ids.len(), 2);
        assert!(!ids.contains("fs:/tmp/a1.txt"));
    }

    #[test]
    fn test_upsert_replaces_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_str().unwrap();

        let mut index = LixunIndex::create_or_open(path, RankingConfig::default()).unwrap();
        let mut writer = index.writer(20_000_000).unwrap();

        let doc1 = sample_document("fs:/tmp/same.txt", "old_title.txt", "old content");
        let doc2 = sample_document("fs:/tmp/same.txt", "new_title.txt", "new content");

        index.upsert(&doc1, &mut writer).unwrap();
        index.upsert(&doc2, &mut writer).unwrap();
        index.commit(&mut writer).unwrap();

        let results = index
            .search(&Query {
                text: "new".to_string(),
                limit: 10,
            })
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "new_title.txt");
    }

    #[test]
    fn test_empty_search_returns_no_results() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_str().unwrap();

        let index = LixunIndex::create_or_open(path, RankingConfig::default()).unwrap();

        let results = index
            .search(&Query {
                text: "".to_string(),
                limit: 10,
            })
            .unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_multiple_documents_search() {
        let docs: Vec<_> = (0..5)
            .map(|i| {
                sample_document(
                    &format!("fs:/tmp/doc{i}.txt"),
                    &format!("doc{i}.txt"),
                    &format!("content number {i}"),
                )
            })
            .collect();

        let (_tmp, index) = create_index_with_docs(&docs);
        let results = search(&index, "content");
        assert_eq!(results.len(), 5);
    }

    #[test]
    fn test_search_limit() {
        let docs: Vec<_> = (0..10)
            .map(|i| {
                sample_document(
                    &format!("fs:/tmp/lim{i}.txt"),
                    &format!("lim{i}.txt"),
                    &format!("limit test {i}"),
                )
            })
            .collect();

        let (_tmp, index) = create_index_with_docs(&docs);

        let results = index
            .search(&Query {
                text: "limit".to_string(),
                limit: 3,
            })
            .unwrap();
        assert!(results.len() <= 3);
    }

    #[test]
    fn test_upsert_and_search_round_trips_icon_and_kind() {
        let mut doc = sample_document("fs:/tmp/roundtrip.pdf", "roundtrip.pdf", "pdf body");
        doc.icon_name = Some("application-pdf".into());
        doc.kind_label = Some("PDF Document".into());

        let (_tmp, index) = create_index_with_docs(&[doc]);
        let results = search(&index, "pdf");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].icon_name.as_deref(), Some("application-pdf"));
        assert_eq!(results[0].kind_label.as_deref(), Some("PDF Document"));
    }

    #[test]
    fn test_search_diacritic_insensitive() {
        let (_tmp_resume, resume_index) =
            create_index_with_docs(&[sample_document("fs:/tmp/resume1.txt", "résumé", "")]);
        let resume_results = search(&resume_index, "resume");
        assert_eq!(resume_results.len(), 1);

        let (_tmp_accented, accented_index) =
            create_index_with_docs(&[sample_document("fs:/tmp/resume2.txt", "resume", "")]);
        let accented_results = search(&accented_index, "résumé");
        assert_eq!(accented_results.len(), 1);
    }

    #[test]
    fn test_search_case_insensitive() {
        let (_tmp, index) =
            create_index_with_docs(&[sample_document("fs:/tmp/firefox.txt", "Firefox", "")]);

        assert_eq!(search(&index, "firefox").len(), 1);
        assert_eq!(search(&index, "FIREFOX").len(), 1);
    }

    #[test]
    fn test_search_camelcase_split() {
        let (_tmp, index) =
            create_index_with_docs(&[sample_document("fs:/tmp/my-file.txt", "MyFileName.txt", "")]);

        assert_eq!(search(&index, "file").len(), 1);
        assert_eq!(search(&index, "name").len(), 1);
    }

    #[test]
    fn test_search_snake_case_split() {
        let (_tmp, index) = create_index_with_docs(&[sample_document(
            "fs:/tmp/report.pdf",
            "my_report_final.pdf",
            "",
        )]);

        assert_eq!(search(&index, "report").len(), 1);
    }

    #[test]
    fn test_search_fuzzy_typo() {
        let (_tmp, index) =
            create_index_with_docs(&[sample_document("fs:/tmp/firefox.txt", "Firefox", "")]);

        assert_eq!(search(&index, "firfox").len(), 1);
        assert!(search(&index, "chrom").is_empty());
    }

    #[test]
    fn test_search_and_default() {
        let docs = vec![
            sample_document("fs:/tmp/foo-bar.txt", "foo bar", ""),
            sample_document("fs:/tmp/foo-baz.txt", "foo baz", ""),
        ];
        let (_tmp, index) = create_index_with_docs(&docs);

        let both_terms = search(&index, "foo bar");
        assert_eq!(both_terms[0].title, "foo bar");

        let single_term = search(&index, "foo");
        assert_eq!(single_term.len(), 2);
    }

    #[test]
    fn test_search_not_operator() {
        let docs = vec![
            sample_document("fs:/tmp/report-2024.txt", "report 2024", ""),
            sample_document("fs:/tmp/draft.txt", "draft report", ""),
        ];
        let (_tmp, index) = create_index_with_docs(&docs);

        let results = search(&index, "report -draft");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "report 2024");
    }

    #[test]
    fn test_search_or_operator() {
        let docs = vec![
            sample_document("fs:/tmp/foo.txt", "foo", ""),
            sample_document("fs:/tmp/bar.txt", "bar", ""),
        ];
        let (_tmp, index) = create_index_with_docs(&docs);

        let results = search(&index, "foo | bar");
        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|hit| hit.title == "foo"));
        assert!(results.iter().any(|hit| hit.title == "bar"));
    }

    #[test]
    fn test_title_boost_outranks_body() {
        let docs = vec![
            sample_document("fs:/tmp/title-urgent.txt", "urgent", "background"),
            sample_document("fs:/tmp/body-urgent.txt", "background", "urgent"),
        ];
        let (_tmp, index) = create_index_with_docs(&docs);

        let results = search(&index, "urgent");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id.0, "fs:/tmp/title-urgent.txt");
        assert!(results[0].score > results[1].score);
    }

    #[test]
    fn test_acronym_retrieval_jsonparser() {
        let docs = vec![sample_document(
            "fs:/tmp/JSONParser.java",
            "JSONParser",
            "class JSONParser parses JSON input",
        )];
        let (_tmp, index) = create_index_with_docs(&docs);

        let results = search(&index, "jp");
        assert_eq!(
            results.len(),
            1,
            "query `jp` must retrieve JSONParser via title_initials"
        );
        assert_eq!(results[0].id.0, "fs:/tmp/JSONParser.java");
    }

    #[test]
    fn test_prefix_retrieval_firefox() {
        let mut firefox = sample_document(
            "app:firefox.desktop",
            "Firefox",
            "Web browser by Mozilla",
        );
        firefox.category = Category::App;
        let docs = vec![firefox];
        let (_tmp, index) = create_index_with_docs(&docs);

        let results = search(&index, "fire");
        assert_eq!(
            results.len(),
            1,
            "query `fire` must retrieve Firefox via title_prefixes"
        );
        assert_eq!(results[0].id.0, "app:firefox.desktop");
    }

    #[test]
    fn test_prefix_ranks_above_body_match() {
        let mut firefox = sample_document(
            "app:firefox.desktop",
            "Firefox",
            "browser application",
        );
        firefox.category = Category::App;
        let notes = sample_document(
            "fs:/tmp/notes.txt",
            "Notes",
            "thoughts about fire safety",
        );
        let (_tmp, index) = create_index_with_docs(&[firefox, notes]);

        let results = search(&index, "fire");
        assert!(
            results.len() >= 2,
            "both docs must match: title-prefix for Firefox, body for Notes"
        );
        assert_eq!(
            results[0].id.0, "app:firefox.desktop",
            "title_prefixes + title boosts must outrank body-only match"
        );
        assert!(results[0].score > results[1].score);
    }

    #[test]
    fn test_create_or_open_rebuilds_index_when_version_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let old_schema = {
            let mut builder = tantivy::schema::Schema::builder();
            let id = builder.add_text_field("id", STRING | STORED);
            let category = builder.add_text_field("category", TEXT | STORED);
            let title = builder.add_text_field("title", TEXT | STORED);
            let subtitle = builder.add_text_field("subtitle", STORED);
            let body = builder.add_text_field("body", TEXT);
            let path = builder.add_text_field("path", TEXT | STORED);
            let mtime = builder.add_i64_field("mtime", STORED);
            let size = builder.add_u64_field("size", STORED);
            let action = builder.add_text_field("action", STORED);
            let extract_fail = builder.add_bool_field("extract_fail", STORED);
            let schema = builder.build();

            let dir = MmapDirectory::open(tmp.path()).unwrap();
            let index =
                Index::create(dir, schema.clone(), tantivy::IndexSettings::default()).unwrap();
            let mut writer = index.writer::<TantivyDocument>(20_000_000).unwrap();
            writer
                .add_document(doc![
                    id => "fs:/tmp/legacy.txt",
                    category => "file",
                    title => "legacy.txt",
                    subtitle => "/tmp/legacy.txt",
                    body => "legacy body",
                    path => "/tmp/legacy.txt",
                    mtime => 0i64,
                    size => 1u64,
                    action => serde_json::to_string(&lixun_core::Action::OpenFile { path: "/tmp/legacy.txt".into() }).unwrap(),
                    extract_fail => false,
                ])
                .unwrap();
            writer.commit().unwrap();
            schema
        };
        assert!(tmp.path().join("meta.json").exists());
        assert!(!tmp.path().join(INDEX_VERSION_FILE).exists());
        drop(old_schema);

        let index =
            LixunIndex::create_or_open(tmp.path().to_str().unwrap(), RankingConfig::default())
                .unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(INDEX_VERSION_FILE))
                .unwrap()
                .trim(),
            INDEX_VERSION.to_string()
        );

        let results = index
            .search(&Query {
                text: "legacy".to_string(),
                limit: 10,
            })
            .unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_index_version_triggers_rebuild() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_str().unwrap();

        let doc = sample_document("fs:/tmp/probe.txt", "probe", "body");
        let (_idx, rebuilt_first) = LixunIndex::create_or_open_with_plugins(
            path,
            &BTreeMap::new(),
            RankingConfig::default(),
        )
        .unwrap();
        assert!(
            rebuilt_first,
            "fresh directory → rebuilt_from_scratch must be true"
        );

        let mut idx = LixunIndex::create_or_open(path, RankingConfig::default()).unwrap();
        let mut writer = idx.writer(20_000_000).unwrap();
        idx.upsert(&doc, &mut writer).unwrap();
        idx.commit(&mut writer).unwrap();
        drop(idx);

        assert_eq!(
            std::fs::read_to_string(tmp.path().join(INDEX_VERSION_FILE))
                .unwrap()
                .trim(),
            INDEX_VERSION.to_string(),
            "version file should hold current INDEX_VERSION after create"
        );

        std::fs::write(
            tmp.path().join(INDEX_VERSION_FILE),
            (INDEX_VERSION - 1).to_string(),
        )
        .unwrap();

        let (_idx2, rebuilt_second) = LixunIndex::create_or_open_with_plugins(
            path,
            &BTreeMap::new(),
            RankingConfig::default(),
        )
        .unwrap();
        assert!(
            rebuilt_second,
            "downgraded version file → rebuilt_from_scratch must be true"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(INDEX_VERSION_FILE))
                .unwrap()
                .trim(),
            INDEX_VERSION.to_string(),
            "rebuild must restore current INDEX_VERSION on disk"
        );
    }

    #[test]
    fn test_get_doc_by_id_roundtrip() {
        let doc = sample_document(
            "fs:/tmp/fetchme.txt",
            "fetchme.txt",
            "the body we want back",
        );
        let (_tmp, index) = create_index_with_docs(&[doc]);

        let got = index
            .get_doc_by_id("fs:/tmp/fetchme.txt")
            .unwrap()
            .expect("document must be found after upsert+commit");

        assert_eq!(got.id.0, "fs:/tmp/fetchme.txt");
        assert_eq!(got.title, "fetchme.txt");
        assert_eq!(got.body.as_deref(), Some("the body we want back"));
        assert_eq!(got.category, Category::File);
    }

    #[test]
    fn test_get_doc_by_id_missing_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_str().unwrap();
        let index = LixunIndex::create_or_open(path, RankingConfig::default()).unwrap();
        assert!(index.get_doc_by_id("fs:/nope").unwrap().is_none());
    }

    #[test]
    fn test_get_body_by_id_returns_body_when_present() {
        let doc = sample_document(
            "fs:/tmp/withbody.txt",
            "withbody.txt",
            "the recovered body from a prior ocr pass",
        );
        let (_tmp, index) = create_index_with_docs(&[doc]);

        let body = index
            .get_body_by_id("fs:/tmp/withbody.txt")
            .unwrap()
            .expect("body must be present");
        assert_eq!(body, "the recovered body from a prior ocr pass");
    }

    #[test]
    fn test_get_body_by_id_returns_none_when_empty_or_missing() {
        let empty = sample_document("fs:/tmp/empty.txt", "empty.txt", "");
        let whitespace = sample_document("fs:/tmp/ws.txt", "ws.txt", "   \t\n  ");
        let (_tmp, index) = create_index_with_docs(&[empty, whitespace]);

        assert!(
            index.get_body_by_id("fs:/tmp/empty.txt").unwrap().is_none(),
            "empty body must surface as None"
        );
        assert!(
            index.get_body_by_id("fs:/tmp/ws.txt").unwrap().is_none(),
            "whitespace-only body must surface as None"
        );
        assert!(
            index.get_body_by_id("fs:/nope").unwrap().is_none(),
            "missing doc must surface as None"
        );
    }

    #[test]
    fn test_ranking_config_category_multiplier() {
        let mut app_doc = sample_document("app:zzz.desktop", "zzz", "");
        app_doc.category = Category::App;
        let file_doc = sample_document("fs:/tmp/zzz.txt", "zzz", "");

        let ranking = RankingConfig {
            apps: 99.0,
            files: 1.0,
            mail: 1.0,
            attachments: 1.0,
            ..Default::default()
        };
        let (_tmp, index) = create_index_with_docs_and_ranking(&[app_doc, file_doc], ranking);

        let hits = index
            .search(&Query {
                text: "zzz".into(),
                limit: 10,
            })
            .unwrap();

        assert_eq!(hits.len(), 2, "both docs should match");
        assert_eq!(hits[0].category, Category::App);
        assert_eq!(hits[1].category, Category::File);
        assert!(
            hits[0].score > hits[1].score * 90.0,
            "apps=99 vs files=1 must amplify top score by >90×: got {} vs {}",
            hits[0].score,
            hits[1].score,
        );
    }
}
