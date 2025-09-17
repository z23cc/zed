use itertools::Itertools as _;
use serde::Serialize;
use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use strum::EnumIter;
use tree_sitter::{QueryCursor, StreamingIterator, Tree};

use crate::{Declaration, outline::Identifier};

#[derive(Clone, Debug)]
pub struct ScoredSnippet {
    #[allow(dead_code)]
    pub identifier: Identifier,
    pub definition_file: Arc<Path>,
    pub definition: OutlineItem,
    pub score_components: ScoreInputs,
    pub scores: Scores,
}

// TODO: Consider having "Concise" style corresponding to `concise_text`
#[derive(EnumIter, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SnippetStyle {
    Signature,
    Definition,
}

impl ScoredSnippet {
    /// Returns the score for this snippet with the specified style.
    pub fn score(&self, style: SnippetStyle) -> f32 {
        match style {
            SnippetStyle::Signature => self.scores.signature,
            SnippetStyle::Definition => self.scores.definition,
        }
    }

    /// Returns the byte range for the snippet with the specified style. For `Signature` this is the
    /// signature_range expanded to line boundaries. For `Definition` this is the item_range expanded to
    /// line boundaries (similar to slice_at_line_boundaries).
    pub fn line_range(
        &self,
        identifier_index: &IdentifierIndex,
        style: SnippetStyle,
    ) -> Range<usize> {
        let source = identifier_index
            .path_to_source
            .get(&self.definition_file)
            .unwrap();

        let base_range = match style {
            SnippetStyle::Signature => self.definition.signature_range.clone(),
            SnippetStyle::Definition => self.definition.item_range.clone(),
        };

        expand_range_to_line_boundaries(source, base_range)
    }

    pub fn score_density(&self, identifier_index: &IdentifierIndex, style: SnippetStyle) -> f32 {
        self.score(style) / range_size(self.line_range(identifier_index, style)) as f32
    }
}

fn scored_snippets(
    language: &Language,
    index: &IdentifierIndex,
    source: &str,
    reference_file: &Path,
    references: Vec<Reference>,
    cursor_offset: usize,
    excerpt_range: Range<usize>,
) -> Vec<ScoredSnippet> {
    let cursor = point_from_offset(source, cursor_offset);

    let containing_range_identifier_occurrences =
        IdentifierOccurrences::within_string(&source[excerpt_range.clone()]);

    let start_point = Point::new(cursor.row.saturating_sub(2), 0);
    let end_point = Point::new(cursor.row + 1, 0);
    let adjacent_identifier_occurrences = IdentifierOccurrences::within_string(
        &source[offset_from_point(source, start_point)..offset_from_point(source, end_point)],
    );

    let mut identifier_to_references: HashMap<Identifier, Vec<Reference>> = HashMap::new();
    for reference in references {
        identifier_to_references
            .entry(reference.identifier.clone())
            .or_insert_with(Vec::new)
            .push(reference);
    }

    identifier_to_references
        .into_iter()
        .flat_map(|(identifier, references)| {
            let Some(definitions) = index
                .identifier_to_definitions
                .get(&(identifier.clone(), language.name.clone()))
            else {
                return Vec::new();
            };
            let definition_count = definitions.len();
            let definition_file_count = definitions.keys().len();

            definitions
                .iter_all()
                .flat_map(|(definition_file, file_definitions)| {
                    let same_file_definition_count = file_definitions.len();
                    let is_same_file = reference_file == definition_file.as_ref();
                    file_definitions
                        .iter()
                        .filter(|definition| {
                            !is_same_file
                                || !range_intersection(&definition.item_range, &excerpt_range)
                                    .is_some()
                        })
                        .filter_map(|definition| {
                            let definition_line_distance = if is_same_file {
                                let definition_line =
                                    point_from_offset(source, definition.item_range.start).row;
                                (cursor.row as i32 - definition_line as i32).abs() as u32
                            } else {
                                0
                            };
                            Some((definition_line_distance, definition))
                        })
                        .sorted_by_key(|&(distance, _)| distance)
                        .enumerate()
                        .map(
                            |(
                                definition_line_distance_rank,
                                (definition_line_distance, definition),
                            )| {
                                score_snippet(
                                    index,
                                    source,
                                    &identifier,
                                    &references,
                                    definition_file.clone(),
                                    definition.clone(),
                                    is_same_file,
                                    definition_line_distance,
                                    definition_line_distance_rank,
                                    same_file_definition_count,
                                    definition_count,
                                    definition_file_count,
                                    &containing_range_identifier_occurrences,
                                    &adjacent_identifier_occurrences,
                                    cursor,
                                )
                            },
                        )
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        })
        .flatten()
        .collect::<Vec<_>>()
}

fn score_snippet(
    index: &IdentifierIndex,
    reference_source: &str,
    identifier: &Identifier,
    references: &Vec<Reference>,
    definition_file: Arc<Path>,
    definition: OutlineItem,
    is_same_file: bool,
    definition_line_distance: u32,
    definition_line_distance_rank: usize,
    same_file_definition_count: usize,
    definition_count: usize,
    definition_file_count: usize,
    containing_range_identifier_occurrences: &IdentifierOccurrences,
    adjacent_identifier_occurrences: &IdentifierOccurrences,
    cursor: Point,
) -> Option<ScoredSnippet> {
    let is_referenced_nearby = references
        .iter()
        .any(|r| r.reference_region == ReferenceRegion::Nearby);
    let is_referenced_in_breadcrumb = references
        .iter()
        .any(|r| r.reference_region == ReferenceRegion::Breadcrumb);
    let reference_count = references.len();
    let reference_line_distance = references
        .iter()
        .map(|r| {
            let reference_line = point_from_offset(reference_source, r.range.start).row as i32;
            (cursor.row as i32 - reference_line).abs() as u32
        })
        .min()
        .unwrap();

    let definition_source = index.path_to_source.get(&definition_file).unwrap();
    let item_source_occurrences =
        IdentifierOccurrences::within_string(definition.item(&definition_source));
    let item_signature_occurrences =
        IdentifierOccurrences::within_string(definition.signature(&definition_source));
    let containing_range_vs_item_jaccard = jaccard_similarity(
        containing_range_identifier_occurrences,
        &item_source_occurrences,
    );
    let containing_range_vs_signature_jaccard = jaccard_similarity(
        containing_range_identifier_occurrences,
        &item_signature_occurrences,
    );
    let adjacent_vs_item_jaccard =
        jaccard_similarity(adjacent_identifier_occurrences, &item_source_occurrences);
    let adjacent_vs_signature_jaccard =
        jaccard_similarity(adjacent_identifier_occurrences, &item_signature_occurrences);

    let containing_range_vs_item_weighted_overlap = weighted_overlap_coefficient(
        containing_range_identifier_occurrences,
        &item_source_occurrences,
    );
    let containing_range_vs_signature_weighted_overlap = weighted_overlap_coefficient(
        containing_range_identifier_occurrences,
        &item_signature_occurrences,
    );
    let adjacent_vs_item_weighted_overlap =
        weighted_overlap_coefficient(adjacent_identifier_occurrences, &item_source_occurrences);
    let adjacent_vs_signature_weighted_overlap =
        weighted_overlap_coefficient(adjacent_identifier_occurrences, &item_signature_occurrences);

    let score_components = ScoreInputs {
        is_same_file,
        is_referenced_nearby,
        is_referenced_in_breadcrumb,
        reference_line_distance,
        definition_line_distance,
        definition_line_distance_rank,
        reference_count,
        same_file_definition_count,
        definition_count,
        definition_file_count,
        containing_range_vs_item_jaccard,
        containing_range_vs_signature_jaccard,
        adjacent_vs_item_jaccard,
        adjacent_vs_signature_jaccard,
        containing_range_vs_item_weighted_overlap,
        containing_range_vs_signature_weighted_overlap,
        adjacent_vs_item_weighted_overlap,
        adjacent_vs_signature_weighted_overlap,
    };

    Some(ScoredSnippet {
        identifier: identifier.clone(),
        definition_file,
        definition,
        scores: score_components.score(),
        score_components,
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct ScoreInputs {
    pub is_same_file: bool,
    pub is_referenced_nearby: bool,
    pub is_referenced_in_breadcrumb: bool,
    pub reference_count: usize,
    pub same_file_definition_count: usize,
    pub definition_count: usize,
    pub definition_file_count: usize,
    pub reference_line_distance: u32,
    pub definition_line_distance: u32,
    pub definition_line_distance_rank: usize,
    pub containing_range_vs_item_jaccard: f32,
    pub containing_range_vs_signature_jaccard: f32,
    pub adjacent_vs_item_jaccard: f32,
    pub adjacent_vs_signature_jaccard: f32,
    pub containing_range_vs_item_weighted_overlap: f32,
    pub containing_range_vs_signature_weighted_overlap: f32,
    pub adjacent_vs_item_weighted_overlap: f32,
    pub adjacent_vs_signature_weighted_overlap: f32,
}

#[derive(Clone, Debug, Serialize)]
pub struct Scores {
    pub signature: f32,
    pub definition: f32,
}

impl ScoreInputs {
    fn score(&self) -> Scores {
        // Score related to how likely this is the correct definition, range 0 to 1
        let accuracy_score = if self.is_same_file {
            // TODO: use definition_line_distance_rank
            (0.5 / self.same_file_definition_count as f32)
                + (0.5 / self.definition_file_count as f32)
        } else {
            1.0 / self.definition_count as f32
        };

        // Score related to the distance between the reference and cursor, range 0 to 1
        let distance_score = if self.is_referenced_nearby {
            1.0 / (1.0 + self.reference_line_distance as f32 / 10.0).powf(2.0)
        } else {
            // same score as ~14 lines away, rationale is to not overly penalize references from parent signatures
            0.5
        };

        // For now instead of linear combination, the scores are just multiplied together.
        let combined_score = 10.0 * accuracy_score * distance_score;

        Scores {
            signature: combined_score * self.containing_range_vs_signature_weighted_overlap,
            // definition score gets boosted both by being multipled by 2 and by there being more
            // weighted overlap.
            definition: 2.0 * combined_score * self.containing_range_vs_item_weighted_overlap,
        }
    }
}
