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
use lancedb::{Connection, Table, connect};
use lixun_mutation::AnnHit;

const TEXT_TABLE: &str = "text_vectors";
const IMAGE_TABLE: &str = "image_vectors";

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
        upsert_one(
            &self.text,
            doc_id,
            source_instance,
            mtime,
            vector,
            self.text_dim,
        )
        .await
    }

    pub async fn upsert_image(
        &self,
        doc_id: &str,
        source_instance: &str,
        mtime: i64,
        vector: &[f32],
    ) -> Result<()> {
        upsert_one(
            &self.image,
            doc_id,
            source_instance,
            mtime,
            vector,
            self.image_dim,
        )
        .await
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

async fn upsert_one(
    table: &Table,
    doc_id: &str,
    source_instance: &str,
    mtime: i64,
    vector: &[f32],
    dim: usize,
) -> Result<()> {
    if vector.len() != dim {
        anyhow::bail!(
            "vector dim mismatch: got {}, expected {dim} (doc_id={doc_id})",
            vector.len()
        );
    }

    let predicate = doc_id_in_predicate(std::slice::from_ref(&doc_id.to_string()));
    table
        .delete(&predicate)
        .await
        .context("lancedb: pre-upsert delete")?;

    let schema = Arc::new(table_schema(dim));
    let values = Arc::new(Float32Array::from(vector.to_vec()));
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
            Arc::new(StringArray::from(vec![doc_id.to_string()])),
            Arc::new(StringArray::from(vec![source_instance.to_string()])),
            Arc::new(Int64Array::from(vec![mtime])),
            Arc::new(vectors),
        ],
    )
    .context("arrow: build RecordBatch")?;
    let iter: Box<dyn RecordBatchReader + Send> = Box::new(RecordBatchIterator::new(
        vec![Ok(batch)].into_iter(),
        schema,
    ));
    table.add(iter).execute().await.context("lancedb: add")?;
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
