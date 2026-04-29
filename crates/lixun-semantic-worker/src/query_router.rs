use lixun_mutation::Modality;

const IMAGE_ANCHORS: &[&str] = &[
    "a photograph",
    "a snapshot of a person or place",
    "a screenshot of a computer screen",
    "a scanned document or receipt",
    "a PDF scan",
    "a diagram or chart",
    "a picture of an object",
    "a selfie or portrait",
    "a logo or icon",
    "a map or floor plan",
];

const TEXT_ANCHORS: &[&str] = &[
    "source code in a programming language",
    "a paragraph of prose",
    "an email message",
    "a configuration file",
    "a markdown document",
    "a chat message conversation",
    "a log file entry",
    "a shell command",
    "an academic paper or article",
    "a list of items",
];

pub struct QueryRouter {
    image_anchors: Vec<Vec<f32>>,
    text_anchors: Vec<Vec<f32>>,
    margin: f32,
}

impl QueryRouter {
    pub fn new(image_anchors: Vec<Vec<f32>>, text_anchors: Vec<Vec<f32>>, margin: f32) -> Self {
        Self {
            image_anchors,
            text_anchors,
            margin,
        }
    }

    pub fn classify(&self, query_embedding: &[f32]) -> Modality {
        let max_image = self
            .image_anchors
            .iter()
            .map(|anchor| cosine_similarity(query_embedding, anchor))
            .fold(f32::NEG_INFINITY, f32::max);

        let max_text = self
            .text_anchors
            .iter()
            .map(|anchor| cosine_similarity(query_embedding, anchor))
            .fold(f32::NEG_INFINITY, f32::max);

        let diff = (max_image - max_text).abs();
        if diff < self.margin {
            Modality::Both
        } else if max_image > max_text {
            Modality::Image
        } else {
            Modality::Text
        }
    }

    pub fn image_anchor_texts() -> &'static [&'static str] {
        IMAGE_ANCHORS
    }

    pub fn text_anchor_texts() -> &'static [&'static str] {
        TEXT_ANCHORS
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (norm_a * norm_b)
}
