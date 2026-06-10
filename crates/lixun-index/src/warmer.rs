//! Fast-field warmer for the launcher hot path.
//!
//! The launcher reads numeric fast fields declared by plugin schemas
//! (`u64`, `i64`, `f64`, `bool`, `date`) every query. Tantivy lazily
//! `mmap`s a fast-field column on first read; without a warmer, that
//! cost lands on the first user keystroke after every commit.
//!
//! [`FastFieldWarmer`] inspects [`Schema`] once at construction and
//! records the name + numeric type of every field flagged as fast.
//! On each searcher generation, [`Warmer::warm`] opens those columns
//! in every segment so the page-cache work happens off the request
//! thread.
//!
//! Field discovery is fully schema-driven — no hardcoded names — so
//! the warmer adapts automatically when plugin schemas add or remove
//! fast fields. Non-numeric fast fields (`Str`, `Bytes`) are recorded
//! and warmed via the typed accessors as well.
//!
//! # Reload policy
//!
//! Pair this warmer with [`tantivy::ReloadPolicy::Manual`]. The owning
//! daemon explicitly drives [`tantivy::IndexReader::reload`] from the
//! indexer's post-commit hook; no filesystem-polling thread is spawned.

use std::sync::Arc;

use tantivy::Searcher;
use tantivy::SegmentReader;
use tantivy::schema::{FieldType, Schema};

/// Numeric / column kind of a fast field, used to dispatch to the
/// correct [`tantivy::fastfield::FastFieldReaders`] accessor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FastFieldKind {
    U64,
    I64,
    F64,
    Bool,
    Date,
    Str,
    Bytes,
    IpAddr,
}

#[derive(Clone, Debug)]
struct FastFieldEntry {
    name: String,
    kind: FastFieldKind,
}

/// Eagerly opens every fast-field column on each new searcher
/// generation so the first user query after a commit pays no
/// page-cache fault penalty.
pub struct FastFieldWarmer {
    fields: Vec<FastFieldEntry>,
}

impl FastFieldWarmer {
    /// Build a warmer from the index schema. Every field whose entry
    /// reports `is_fast() == true` is recorded for warming.
    pub fn from_schema(schema: &Schema) -> Arc<Self> {
        let mut fields = Vec::new();
        for (_, entry) in schema.fields() {
            if !entry.is_fast() {
                continue;
            }
            let kind = match entry.field_type() {
                FieldType::U64(_) => FastFieldKind::U64,
                FieldType::I64(_) => FastFieldKind::I64,
                FieldType::F64(_) => FastFieldKind::F64,
                FieldType::Bool(_) => FastFieldKind::Bool,
                FieldType::Date(_) => FastFieldKind::Date,
                FieldType::Str(_) => FastFieldKind::Str,
                FieldType::Bytes(_) => FastFieldKind::Bytes,
                FieldType::IpAddr(_) => FastFieldKind::IpAddr,
                // JsonObject fast fields exist in tantivy 0.26 but
                // are not yet used by any lixun schema. Skip rather
                // than guess at the right accessor.
                FieldType::JsonObject(_) => continue,
                // Facet is not fast-field eligible; defensive skip.
                FieldType::Facet(_) => continue,
            };
            fields.push(FastFieldEntry {
                name: entry.name().to_string(),
                kind,
            });
        }
        Arc::new(Self { fields })
    }

    /// Number of fast fields the warmer will touch per segment.
    /// Useful for tests and tracing.
    #[allow(dead_code)]
    pub fn field_count(&self) -> usize {
        self.fields.len()
    }

    fn warm_segment(&self, segment: &SegmentReader) -> tantivy::Result<()> {
        let fast_fields = segment.fast_fields();
        for entry in &self.fields {
            match entry.kind {
                FastFieldKind::U64 => {
                    let _ = fast_fields.u64(&entry.name)?;
                }
                FastFieldKind::I64 => {
                    let _ = fast_fields.i64(&entry.name)?;
                }
                FastFieldKind::F64 => {
                    let _ = fast_fields.f64(&entry.name)?;
                }
                FastFieldKind::Bool => {
                    let _ = fast_fields.bool(&entry.name)?;
                }
                FastFieldKind::Date => {
                    let _ = fast_fields.date(&entry.name)?;
                }
                FastFieldKind::Str => {
                    let _ = fast_fields.str(&entry.name)?;
                }
                FastFieldKind::Bytes => {
                    let _ = fast_fields.bytes(&entry.name)?;
                }
                FastFieldKind::IpAddr => {
                    let _ = fast_fields.ip_addr(&entry.name)?;
                }
            }
        }
        Ok(())
    }
}

impl tantivy::Warmer for FastFieldWarmer {
    fn warm(&self, searcher: &Searcher) -> tantivy::Result<()> {
        if self.fields.is_empty() {
            return Ok(());
        }
        for segment in searcher.segment_readers() {
            self.warm_segment(segment)?;
        }
        Ok(())
    }

    fn garbage_collect(&self, _live_generations: &[&tantivy::SearcherGeneration]) {
        // TODO(future): per-generation cache GC. The warmer holds no
        // per-generation state today — columns are mmap'd into the
        // segment reader, which Tantivy already drops when no live
        // searcher references it. A future cache layer keyed by
        // generation would clear stale entries here.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::schema::{NumericOptions, STORED, TEXT};

    #[test]
    fn picks_up_fast_fields_only() {
        let mut builder = Schema::builder();
        builder.add_text_field("title", TEXT | STORED);
        builder.add_i64_field("mtime", STORED);
        builder.add_u64_field("rank", NumericOptions::default().set_indexed().set_fast());
        builder.add_i64_field("score", NumericOptions::default().set_indexed().set_fast());
        let schema = builder.build();

        let warmer = FastFieldWarmer::from_schema(&schema);
        assert_eq!(warmer.field_count(), 2, "only fast fields should be picked");
    }

    #[test]
    fn empty_schema_has_no_fast_fields() {
        let mut builder = Schema::builder();
        builder.add_text_field("title", TEXT | STORED);
        let schema = builder.build();

        let warmer = FastFieldWarmer::from_schema(&schema);
        assert_eq!(warmer.field_count(), 0);
    }
}
