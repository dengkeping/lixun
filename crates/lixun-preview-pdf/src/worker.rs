//! Public render-job data types. These were originally defined for
//! the per-worker thread architecture; rendering itself now happens
//! inside [`crate::poppler_host`], which re-exports these shapes
//! verbatim so the rest of the crate (canvas, page widget, session)
//! does not need to know how rendering is wired.

#[derive(Debug, Clone, Copy)]
pub struct RenderJob {
    pub page_index: u32,
    pub zoom_bucket: u32,
    pub epoch: u64,
}

pub struct RenderResult {
    pub page_index: u32,
    pub zoom_bucket: u32,
    pub epoch: u64,
    pub outcome: RenderOutcome,
}

pub enum RenderOutcome {
    Ok {
        texture: gdk::MemoryTexture,
        width: u32,
        height: u32,
        bytes: usize,
    },
    Err(String),
}
