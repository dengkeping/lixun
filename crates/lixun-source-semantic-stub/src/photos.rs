use std::collections::{HashMap, HashSet};

use super::source::SemanticIpcAnnHandle;
use lixun_core::{Action, Category, DocId, Hit, RowMenuDef};
use lixun_mutation::RankedPage;
use lixun_mutation::{AnnHit, Modality};

const RELATED: &str = "related:";
// Squared L2 on the unit-normalized vision vectors. See docs/search-quality.md.
const BEST_DISTANCE: f32 = 1.48;
const MATCH_DISTANCE: f32 = 1.51;
const SCREEN_MARGIN: f32 = 0.04;

pub(super) struct PhotoQuery<'a> {
    text: &'a str,
    related: bool,
    subject: String,
}

impl<'a> PhotoQuery<'a> {
    pub(super) fn parse(raw: &'a str) -> Option<Self> {
        let raw = raw.trim();
        let text = raw.strip_prefix(RELATED).unwrap_or(raw).trim();
        if lixun_core::query_uses_operators(text) {
            return None;
        }
        let lower = text.to_lowercase();
        Some(Self {
            text,
            related: raw != text,
            subject: photo_subject(&lower)?.to_owned(),
        })
    }

    pub(super) async fn search(
        &self,
        ann: &SemanticIpcAnnHandle,
        k: usize,
    ) -> anyhow::Result<Option<RankedPage>> {
        if self.related {
            let images = ann.image_neighbours(self.text, k).await?;
            return Ok(Some(RankedPage {
                ids: images.into_iter().map(|h| h.doc_id).collect(),
                empty_suggestion: None,
            }));
        }
        let (images, screens) = tokio::try_join!(
            ann.image_neighbours(self.text, k),
            ann.image_neighbours("a screenshot of text", k)
        )?;
        let excluded = screen_matches(&images, &screens);
        let mut candidates = Self::select(&images, &excluded);
        if candidates
            .first()
            .is_some_and(|best| best.distance > BEST_DISTANCE)
        {
            let mut subject_hits = ann.image_neighbours(&self.subject, k).await?;
            subject_hits.retain(|h| !excluded.contains(h.doc_id.as_str()));
            corroborate(&mut candidates, &subject_hits);
        }
        let ids = candidates.into_iter().map(|h| h.doc_id.clone()).collect();
        Ok(Some(RankedPage {
            ids,
            empty_suggestion: (!images.is_empty()).then(|| self.expand_action()),
        }))
    }

    fn select<'b>(images: &'b [AnnHit], excluded: &HashSet<&str>) -> Vec<&'b AnnHit> {
        images
            .iter()
            .filter(|h| h.distance <= MATCH_DISTANCE && !excluded.contains(h.doc_id.as_str()))
            .collect()
    }

    fn expand_action(&self) -> Hit {
        let q = format!("{RELATED}{}", self.text);
        Hit {
            id: DocId(format!("visual-query:{q}")),
            category: Category::File,
            title: "No confident photo matches".into(),
            subtitle: "Show related images".into(),
            action: Action::ReplaceQuery { q },
            icon_name: Some("image-x-generic-symbolic".into()),
            score: 1.0,
            kind_label: None,
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
            secondary_action: None,
            source_instance: String::new(),
            row_menu: RowMenuDef::empty(),
            mime: None,
            timestamp: None,
            size: None,
        }
    }
}

// Apply the same screenshot exclusions to both rankings.
fn screen_matches<'a>(images: &'a [AnnHit], screens: &[AnnHit]) -> HashSet<&'a str> {
    let distances: HashMap<_, _> = screens
        .iter()
        .map(|h| (h.doc_id.as_str(), h.distance))
        .collect();
    images
        .iter()
        .filter(|h| h.distance <= MATCH_DISTANCE)
        .filter(|h| {
            distances
                .get(h.doc_id.as_str())
                .is_some_and(|&d| d <= h.distance + SCREEN_MARGIN)
        })
        .map(|h| h.doc_id.as_str())
        .collect()
}

/// Only the first image has independent rank confirmation on a borderline page.
fn corroborate(candidates: &mut Vec<&AnnHit>, subject_hits: &[AnnHit]) {
    let agreed = candidates
        .first()
        .zip(subject_hits.first())
        .is_some_and(|(primary, subject)| primary.doc_id == subject.doc_id);
    candidates.truncate(usize::from(agreed));
}

fn photo_subject(text: &str) -> Option<&str> {
    if looks_like_path(text) {
        return None;
    }
    let photos = [
        "photo of ",
        "photos of ",
        "photograph of ",
        "photographs of ",
        "pictures of ",
        "picture of ",
        "фото ",
        "фотография ",
        "фотографии ",
    ];
    photos
        .iter()
        .find_map(|prefix| text.strip_prefix(prefix))
        .or_else(|| text.strip_suffix("的照片"))
        .map(str::trim)
        .filter(|subject| !subject.is_empty())
}

fn looks_like_path(text: &str) -> bool {
    !text.contains(char::is_whitespace) && text.contains(['-', '_', '.', '/'])
}

pub(crate) fn explicit_media_intent(query: &str) -> Option<Modality> {
    let text = query.trim().to_lowercase();
    if photo_subject(&text).is_some() {
        return Some(Modality::Image);
    }
    if looks_like_path(&text) {
        return None;
    }
    if [
        "article about ",
        "articles about ",
        "статья о ",
        "статья об ",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
        || text.ends_with("的文章")
    {
        Some(Modality::Text)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(name: &str, distance: f32) -> AnnHit {
        AnnHit {
            doc_id: name.into(),
            distance,
        }
    }

    #[test]
    fn borderline_photos_require_agreement_on_the_leading_candidate() {
        let images = [
            hit("screenshot", 1.49),
            hit("first", 1.499),
            hit("second", 1.509),
            hit("unconfirmed", 1.51),
            hit("too far", 1.52),
        ];
        let screens = screen_matches(&images, &[hit("screenshot", 1.46)]);
        let candidates = PhotoQuery::select(&images, &screens);
        assert!(candidates[0].distance > BEST_DISTANCE);
        let mut agreed = candidates.clone();
        corroborate(
            &mut agreed,
            &[hit("first", 1.50), hit("other", 1.51), hit("second", 1.52)],
        );
        assert_eq!(
            agreed.iter().map(|h| h.doc_id.as_str()).collect::<Vec<_>>(),
            ["first"]
        );
        let mut disagreed = candidates;
        corroborate(&mut disagreed, &[hit("other", 1.50), hit("first", 1.51)]);
        assert!(disagreed.is_empty());
    }

    #[test]
    fn subject_keeps_modifiers_and_respects_search_syntax() {
        for (q, subject) in [
            ("Photos of black cats", "black cats"),
            ("ФОТО белых кошек", "белых кошек"),
            ("黑猫的照片", "黑猫"),
        ] {
            assert_eq!(PhotoQuery::parse(q).unwrap().subject, subject);
        }
        assert!(PhotoQuery::parse("photos of cats ext:pdf").is_none());
        assert!(PhotoQuery::parse("album/猫的照片").is_none());
    }
}
