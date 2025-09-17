use collections::HashSet;
use gpui::{App, Entity};
use itertools::Itertools as _;
use language::BufferSnapshot;
use project::ProjectEntryId;
use serde::Serialize;
use std::{collections::HashMap, ops::Range};
use strum::EnumIter;
use text::{OffsetRangeExt, Point, ToPoint};

use crate::{
    Declaration, EditPredictionExcerpt, EditPredictionExcerptText, TreeSitterIndex,
    outline::Identifier,
    reference::{Reference, ReferenceRegion},
    text_similarity::{IdentifierOccurrences, jaccard_similarity, weighted_overlap_coefficient},
};

#[derive(Clone, Debug)]
pub struct ScoredSnippet {
    #[allow(dead_code)]
    pub identifier: Identifier,
    pub declaration: Declaration,
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

    pub fn size(&self, style: SnippetStyle) -> usize {
        todo!()
    }

    pub fn score_density(&self, style: SnippetStyle) -> f32 {
        self.score(style) / (self.size(style)) as f32
    }
}

fn scored_snippets(
    index: Entity<TreeSitterIndex>,
    excerpt: &EditPredictionExcerpt,
    excerpt_text: &EditPredictionExcerptText,
    references: Vec<Reference>,
    cursor_offset: usize,
    current_buffer: &BufferSnapshot,
    cx: &App,
) -> Vec<ScoredSnippet> {
    let containing_range_identifier_occurrences =
        IdentifierOccurrences::within_string(&excerpt_text.body);
    let cursor_point = cursor_offset.to_point(&current_buffer);

    // todo! ask michael why we needed this
    // if let Some(cursor_within_excerpt) = cursor_offset.checked_sub(excerpt.range.start) {
    // } else {
    // };
    let start_point = Point::new(cursor_point.row.saturating_sub(2), 0);
    let end_point = Point::new(cursor_point.row + 1, 0);
    let adjacent_identifier_occurrences = IdentifierOccurrences::within_string(
        &current_buffer
            .text_for_range(start_point..end_point)
            .collect::<String>(),
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
            let definitions = index
                .read(cx)
                // todo! pick a limit
                .declarations_for_identifier::<16>(&identifier, cx);
            let definition_count = definitions.len();
            let total_file_count = definitions
                .iter()
                .filter_map(|definition| definition.project_entry_id(cx))
                .collect::<HashSet<ProjectEntryId>>()
                .len();

            definitions
                .iter()
                .filter_map(|definition| match definition {
                    Declaration::Buffer {
                        declaration,
                        buffer,
                    } => {
                        let is_same_file = buffer
                            .read_with(cx, |buffer, _| buffer.remote_id())
                            .is_ok_and(|buffer_id| buffer_id == current_buffer.remote_id());

                        if is_same_file {
                            range_intersection(
                                &declaration.item_range.to_offset(&current_buffer),
                                &excerpt.range,
                            )
                            .is_none()
                            .then(|| {
                                let definition_line =
                                    declaration.item_range.start.to_point(current_buffer).row;
                                (
                                    true,
                                    (cursor_point.row as i32 - definition_line as i32).abs() as u32,
                                    definition,
                                )
                            })
                        } else {
                            Some((false, 0, definition))
                        }
                    }
                    Declaration::File { .. } => {
                        // We can assume that a file declaration is in a different file,
                        // because the current onemust be open
                        Some((false, 0, definition))
                    }
                })
                .sorted_by_key(|&(_, distance, _)| distance)
                .enumerate()
                .map(
                    |(
                        definition_line_distance_rank,
                        (is_same_file, definition_line_distance, definition),
                    )| {
                        let same_file_definition_count =
                            index.read(cx).file_declaration_count(definition);

                        score_snippet(
                            &identifier,
                            &references,
                            definition.clone(),
                            is_same_file,
                            definition_line_distance,
                            definition_line_distance_rank,
                            same_file_definition_count,
                            definition_count,
                            total_file_count,
                            &containing_range_identifier_occurrences,
                            &adjacent_identifier_occurrences,
                            cursor_point,
                            current_buffer,
                            cx,
                        )
                    },
                )
                .collect::<Vec<_>>()
        })
        .flatten()
        .collect::<Vec<_>>()
}

// todo! replace with existing util?
fn range_intersection<T: Ord + Clone>(a: &Range<T>, b: &Range<T>) -> Option<Range<T>> {
    let start = a.start.clone().max(b.start.clone());
    let end = a.end.clone().min(b.end.clone());
    if start < end {
        Some(Range { start, end })
    } else {
        None
    }
}

fn score_snippet(
    identifier: &Identifier,
    references: &[Reference],
    definition: Declaration,
    is_same_file: bool,
    definition_line_distance: u32,
    definition_line_distance_rank: usize,
    same_file_definition_count: usize,
    definition_count: usize,
    definition_file_count: usize,
    containing_range_identifier_occurrences: &IdentifierOccurrences,
    adjacent_identifier_occurrences: &IdentifierOccurrences,
    cursor: Point,
    current_buffer: &BufferSnapshot,
    cx: &App,
) -> Option<ScoredSnippet> {
    let is_referenced_nearby = references
        .iter()
        .any(|r| r.region == ReferenceRegion::Nearby);
    let is_referenced_in_breadcrumb = references
        .iter()
        .any(|r| r.region == ReferenceRegion::Breadcrumb);
    let reference_count = references.len();
    let reference_line_distance = references
        .iter()
        .map(|r| {
            let reference_line = r.range.start.to_point(current_buffer).row as i32;
            (cursor.row as i32 - reference_line).abs() as u32
        })
        .min()
        .unwrap();

    let item_source_occurrences = IdentifierOccurrences::within_string(&definition.item_text(cx));
    let item_signature_occurrences =
        IdentifierOccurrences::within_string(&definition.signature_text(cx));
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
        declaration: definition,
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
    // todo! do we need this?
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
