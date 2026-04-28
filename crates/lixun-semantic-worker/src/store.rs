use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use arrow_array::{
    FixedSizeListArray, Float32Array, Int64Array, RecordBatch, RecordBatchIterator,
    RecordBatchReader, StringArray,
};
use arrow_schema::{DataType, Field, Schema};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::table::{Duration as LanceDuration, OptimizeAction};
use lancedb::{Connection, Table, connect};
use lixun_mutation::AnnHit;

const TEXT_TABLE: &str = "text_vectors";
const IMAGE_TABLE: &str = "image_vectors";

#[derive(Clone, Debug)]
pub struct VectorRow {
    pub doc_id: String,
    pub source_instance: String,
    pub mtime: i64,
    pub vector: Vec<f32>,
}

pub struct VectorStore {
    #[allow(dead_code)]
    conn: Connection,
    text: Table,
    image: Table,
    text_dim: usize,
    image_dim: usize,
}

impl VectorStore {
    pub async fn open(root: &Path, text_dim: usize, image_dim: usize) -> Result<Self> {
        tokio::fs::create_dir_all(root)
            .await
            .with_context(|| format!("creating vector dir {}", root.display()))?;
        let uri = root.to_string_lossy().to_string();
        let conn = connect(&uri)
            .execute()
            .await
            .with_context(|| format!("lancedb: connect {uri}"))?;

        let text = ensure_table(&conn, TEXT_TABLE, text_dim).await?;
        let image = ensure_table(&conn, IMAGE_TABLE, image_dim).await?;

        Ok(Self {
            conn,
            text,
            image,
            text_dim,
            image_dim,
        })
    }

    pub fn text_dim(&self) -> usize {
        self.text_dim
    }

    pub fn image_dim(&self) -> usize {
        self.image_dim
    }

    pub async fn upsert_text(
        &self,
        doc_id: &str,
        source_instance: &str,
        mtime: i64,
        vector: &[f32],
    ) -> Result<()> {
        self.upsert_text_batch(std::slice::from_ref(&VectorRow {
            doc_id: doc_id.to_string(),
            source_instance: source_instance.to_string(),
            mtime,
            vector: vector.to_vec(),
        }))
        .await
    }

    pub async fn upsert_image(
        &self,
        doc_id: &str,
        source_instance: &str,
        mtime: i64,
        vector: &[f32],
    ) -> Result<()> {
        self.upsert_image_batch(std::slice::from_ref(&VectorRow {
            doc_id: doc_id.to_string(),
            source_instance: source_instance.to_string(),
            mtime,
            vector: vector.to_vec(),
        }))
        .await
    }

    pub async fn upsert_text_batch(&self, rows: &[VectorRow]) -> Result<()> {
        upsert_many(&self.text, rows, self.text_dim).await
    }

    pub async fn upsert_image_batch(&self, rows: &[VectorRow]) -> Result<()> {
        upsert_many(&self.image, rows, self.image_dim).await
    }

    /// Run LanceDB's full optimisation pass on both tables: compact
    /// fragments, prune old versions, and refresh indices. Idempotent
    /// and cheap when nothing needs work, so it is safe to call from
    /// the open path to recover from a previously-uncompacted table
    /// (each upsert before the batch API created its own version).
    pub async fn compact_all(&self) -> Result<()> {
        for (name, table) in [("text", &self.text), ("image", &self.image)] {
            table
                .optimize(OptimizeAction::Compact {
                    options: Default::default(),
                    remap_options: None,
                })
                .await
                .with_context(|| format!("lancedb: compact {name}"))?;
            table
                .optimize(OptimizeAction::Prune {
                    older_than: Some(LanceDuration::seconds(0)),
                    delete_unverified: Some(true),
                    error_if_tagged_old_versions: Some(false),
                })
                .await
                .with_context(|| format!("lancedb: prune {name}"))?;
        }
        Ok(())
    }

    /// Compact + prune only when a table has accumulated more than
    /// `fragment_threshold` fragments. Reading the fragment count is
    /// O(1) so the guard itself is free. Fragments climb with each
    /// upsert and drop after compaction, so this metric rebases
    /// after each compaction (unlike `version()`, which is a
    /// monotonic commit counter and never resets). The threshold
    /// avoids paying the compaction working-set cost (a transient
    /// RSS spike of ~1 GB while Lance rewrites fragments) on
    /// healthy tables.
    pub async fn compact_if_stale(&self, fragment_threshold: usize) -> Result<bool> {
        let mut did_work = false;
        for (name, table) in [("text", &self.text), ("image", &self.image)] {
            let frags = table
                .stats()
                .await
                .with_context(|| format!("lancedb: stats({name})"))?
                .fragment_stats
                .num_fragments;
            if frags > fragment_threshold {
                tracing::info!(
                    table = name,
                    fragments = frags,
                    threshold = fragment_threshold,
                    "semantic: compacting stale lance table"
                );
                table
                    .optimize(OptimizeAction::Compact {
                        options: Default::default(),
                        remap_options: None,
                    })
                    .await
                    .with_context(|| format!("lancedb: compact {name}"))?;
                table
                    .optimize(OptimizeAction::Prune {
                        older_than: Some(LanceDuration::seconds(0)),
                        delete_unverified: Some(true),
                        error_if_tagged_old_versions: Some(false),
                    })
                    .await
                    .with_context(|| format!("lancedb: prune {name}"))?;
                did_work = true;
            }
        }
        Ok(did_work)
    }

    pub async fn delete(&self, doc_ids: &[String]) -> Result<()> {
        if doc_ids.is_empty() {
            return Ok(());
        }
        let predicate = doc_id_in_predicate(doc_ids);
        self.text
            .delete(&predicate)
            .await
            .context("lancedb: delete text_vectors")?;
        self.image
            .delete(&predicate)
            .await
            .context("lancedb: delete image_vectors")?;
        Ok(())
    }

    pub async fn search_text(&self, query_vec: &[f32], k: usize) -> Result<Vec<AnnHit>> {
        knn(&self.text, query_vec, k).await
    }

    pub async fn search_image(&self, query_vec: &[f32], k: usize) -> Result<Vec<AnnHit>> {
        knn(&self.image, query_vec, k).await
    }
}

fn vector_field(dim: usize) -> Field {
    Field::new(
        "vector",
        DataType::FixedSizeList(
            Arc::new(Field::new("item", DataType::Float32, true)),
            dim as i32,
        ),
        false,
    )
}

fn table_schema(dim: usize) -> Schema {
    Schema::new(vec![
        Field::new("doc_id", DataType::Utf8, false),
        Field::new("source_instance", DataType::Utf8, false),
        Field::new("mtime", DataType::Int64, false),
        vector_field(dim),
    ])
}

async fn ensure_table(conn: &Connection, name: &str, dim: usize) -> Result<Table> {
    let names = conn
        .table_names()
        .execute()
        .await
        .context("lancedb: table_names")?;
    if names.iter().any(|n| n == name) {
        return conn
            .open_table(name)
            .execute()
            .await
            .with_context(|| format!("lancedb: open_table {name}"));
    }
    let schema = Arc::new(table_schema(dim));
    let empty = RecordBatch::new_empty(schema.clone());
    let reader: Box<dyn RecordBatchReader + Send> = Box::new(RecordBatchIterator::new(
        vec![Ok(empty)].into_iter(),
        schema,
    ));
    conn.create_table(name, reader)
        .execute()
        .await
        .with_context(|| format!("lancedb: create_table {name}"))
}

fn doc_id_in_predicate(ids: &[String]) -> String {
    let list = ids
        .iter()
        .map(|id| format!("'{}'", id.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",");
    format!("doc_id IN ({list})")
}

async fn upsert_many(table: &Table, rows: &[VectorRow], dim: usize) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    for r in rows {
        if r.vector.len() != dim {
            anyhow::bail!(
                "vector dim mismatch: got {}, expected {dim} (doc_id={})",
                r.vector.len(),
                r.doc_id
            );
        }
    }

    let schema = Arc::new(table_schema(dim));
    let n = rows.len();
    let mut doc_ids = Vec::with_capacity(n);
    let mut source_instances = Vec::with_capacity(n);
    let mut mtimes = Vec::with_capacity(n);
    let mut flat_vectors = Vec::with_capacity(n * dim);
    for r in rows {
        doc_ids.push(r.doc_id.clone());
        source_instances.push(r.source_instance.clone());
        mtimes.push(r.mtime);
        flat_vectors.extend_from_slice(&r.vector);
    }

    let values = Arc::new(Float32Array::from(flat_vectors));
    let vectors = FixedSizeListArray::try_new(
        Arc::new(Field::new("item", DataType::Float32, true)),
        dim as i32,
        values,
        None,
    )
    .context("arrow: build FixedSizeList")?;
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(doc_ids)),
            Arc::new(StringArray::from(source_instances)),
            Arc::new(Int64Array::from(mtimes)),
            Arc::new(vectors),
        ],
    )
    .context("arrow: build RecordBatch")?;
    let reader: Box<dyn RecordBatchReader + Send> = Box::new(RecordBatchIterator::new(
        vec![Ok(batch)].into_iter(),
        schema,
    ));

    let mut merge = table.merge_insert(&["doc_id"]);
    merge
        .when_matched_update_all(None)
        .when_not_matched_insert_all();
    merge
        .execute(reader)
        .await
        .context("lancedb: merge_insert")?;
    Ok(())
}

async fn knn(table: &Table, query: &[f32], k: usize) -> Result<Vec<AnnHit>> {
    let stream = table
        .vector_search(query.to_vec())
        .context("lancedb: vector_search build")?
        .limit(k)
        .execute()
        .await
        .context("lancedb: vector_search execute")?;
    let batches: Vec<RecordBatch> = stream
        .try_collect()
        .await
        .context("lancedb: collect knn batches")?;
    let mut out = Vec::new();
    for b in &batches {
        let ids = b
            .column_by_name("doc_id")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
            .context("knn result: doc_id column missing")?;
        let dists = b
            .column_by_name("_distance")
            .and_then(|c| c.as_any().downcast_ref::<Float32Array>())
            .context("knn result: _distance column missing")?;
        for i in 0..b.num_rows() {
            out.push(AnnHit {
                doc_id: ids.value(i).to_string(),
                distance: dists.value(i),
            });
        }
    }
    Ok(out)
}
