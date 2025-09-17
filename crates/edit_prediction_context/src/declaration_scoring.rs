use gpui::{App, Entity};
use itertools::Itertools as _;
use language::BufferSnapshot;
use serde::Serialize;
use std::{collections::HashMap, ops::Range};
use strum::EnumIter;
use text::{OffsetRangeExt, Point, ToPoint};

use crate::{
    Declaration, EditPredictionExcerpt, EditPredictionExcerptText, Identifier, SyntaxIndex,
    reference::{Reference, ReferenceRegion},
    text_similarity::{IdentifierOccurrences, jaccard_similarity, weighted_overlap_coefficient},
};

// TODO:
//
// * Consider adding declaration_file_count (n)

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
    Declaration,
}

impl ScoredSnippet {
    /// Returns the score for this snippet with the specified style.
    pub fn score(&self, style: SnippetStyle) -> f32 {
        match style {
            SnippetStyle::Signature => self.scores.signature,
            SnippetStyle::Declaration => self.scores.declaration,
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
    index: Entity<SyntaxIndex>,
    excerpt: &EditPredictionExcerpt,
    excerpt_text: &EditPredictionExcerptText,
    identifier_to_references: HashMap<Identifier, Vec<Reference>>,
    cursor_offset: usize,
    current_buffer: &BufferSnapshot,
    cx: &App,
) -> Vec<ScoredSnippet> {
    let containing_range_identifier_occurrences =
        IdentifierOccurrences::within_string(&excerpt_text.body);
    let cursor_point = cursor_offset.to_point(&current_buffer);

    let start_point = Point::new(cursor_point.row.saturating_sub(2), 0);
    let end_point = Point::new(cursor_point.row + 1, 0);
    let adjacent_identifier_occurrences = IdentifierOccurrences::within_string(
        &current_buffer
            .text_for_range(start_point..end_point)
            .collect::<String>(),
    );

    identifier_to_references
        .into_iter()
        .flat_map(|(identifier, references)| {
            let declarations = index
                .read(cx)
                // todo! pick a limit
                .declarations_for_identifier::<16>(&identifier, cx);
            let declaration_count = declarations.len();

            declarations
                .iter()
                .filter_map(|declaration| match declaration {
                    Declaration::Buffer {
                        declaration: buffer_declaration,
                        buffer,
                    } => {
                        let is_same_file = buffer
                            .read_with(cx, |buffer, _| buffer.remote_id())
                            .is_ok_and(|buffer_id| buffer_id == current_buffer.remote_id());

                        if is_same_file {
                            range_intersection(
                                &buffer_declaration.item_range.to_offset(&current_buffer),
                                &excerpt.range,
                            )
                            .is_none()
                            .then(|| {
                                let declaration_line = buffer_declaration
                                    .item_range
                                    .start
                                    .to_point(current_buffer)
                                    .row;
                                (
                                    true,
                                    (cursor_point.row as i32 - declaration_line as i32).abs()
                                        as u32,
                                    declaration,
                                )
                            })
                        } else {
                            Some((false, 0, declaration))
                        }
                    }
                    Declaration::File { .. } => {
                        // We can assume that a file declaration is in a different file,
                        // because the current one must be open
                        Some((false, 0, declaration))
                    }
                })
                .sorted_by_key(|&(_, distance, _)| distance)
                .enumerate()
                .map(
                    |(
                        declaration_line_distance_rank,
                        (is_same_file, declaration_line_distance, declaration),
                    )| {
                        let same_file_declaration_count =
                            index.read(cx).file_declaration_count(declaration);

                        score_snippet(
                            &identifier,
                            &references,
                            declaration.clone(),
                            is_same_file,
                            declaration_line_distance,
                            declaration_line_distance_rank,
                            same_file_declaration_count,
                            declaration_count,
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
    declaration: Declaration,
    is_same_file: bool,
    declaration_line_distance: u32,
    declaration_line_distance_rank: usize,
    same_file_declaration_count: usize,
    declaration_count: usize,
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

    let item_source_occurrences =
        IdentifierOccurrences::within_string(&declaration.item_text(cx).0);
    let item_signature_occurrences =
        IdentifierOccurrences::within_string(&declaration.signature_text(cx).0);
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
        declaration_line_distance,
        declaration_line_distance_rank,
        reference_count,
        same_file_declaration_count,
        declaration_count,
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
        declaration: declaration,
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
    pub same_file_declaration_count: usize,
    pub declaration_count: usize,
    pub reference_line_distance: u32,
    pub declaration_line_distance: u32,
    pub declaration_line_distance_rank: usize,
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
    pub declaration: f32,
}

impl ScoreInputs {
    fn score(&self) -> Scores {
        // Score related to how likely this is the correct declaration, range 0 to 1
        let accuracy_score = if self.is_same_file {
            // TODO: use declaration_line_distance_rank
            (0.5 / self.same_file_declaration_count as f32)
        } else {
            1.0 / self.declaration_count as f32
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
            // declaration score gets boosted both by being multipled by 2 and by there being more
            // weighted overlap.
            declaration: 2.0 * combined_score * self.containing_range_vs_item_weighted_overlap,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use gpui::{TestAppContext, prelude::*};
    use indoc::indoc;
    use language::{Language, LanguageConfig, LanguageId, LanguageMatcher, tree_sitter_rust};
    use project::{FakeFs, Project};
    use serde_json::json;
    use settings::SettingsStore;
    use text::ToOffset;
    use util::path;

    use crate::{EditPredictionExcerptOptions, references_in_excerpt};

    #[gpui::test]
    async fn test_call_site(cx: &mut TestAppContext) {
        let (project, index, _rust_lang_id) = init_test(cx).await;

        let buffer = project
            .update(cx, |project, cx| {
                let project_path = project.find_project_path("c.rs", cx).unwrap();
                project.open_buffer(project_path, cx)
            })
            .await
            .unwrap();

        cx.run_until_parked();

        // first process_data call site
        let cursor_point = language::Point::new(8, 21);
        let buffer_snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
        let excerpt = EditPredictionExcerpt::select_from_buffer(
            cursor_point,
            &buffer_snapshot,
            &EditPredictionExcerptOptions {
                max_bytes: 40,
                min_bytes: 10,
                target_before_cursor_over_total_bytes: 0.5,
                include_parent_signatures: false,
            },
        )
        .unwrap();
        let excerpt_text = excerpt.text(&buffer_snapshot);
        let references = references_in_excerpt(&excerpt, &excerpt_text, &buffer_snapshot);
        let cursor_offset = cursor_point.to_offset(&buffer_snapshot);

        let snippets = cx.update(|cx| {
            scored_snippets(
                index,
                &excerpt,
                &excerpt_text,
                references,
                cursor_offset,
                &buffer_snapshot,
                cx,
            )
        });

        assert_eq!(snippets.len(), 1);
        assert_eq!(snippets[0].identifier.name.as_ref(), "process_data");
        drop(buffer);
    }

    async fn init_test(
        cx: &mut TestAppContext,
    ) -> (Entity<Project>, Entity<SyntaxIndex>, LanguageId) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            language::init(cx);
            Project::init_settings(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "a.rs": indoc! {r#"
                    fn main() {
                        let x = 1;
                        let y = 2;
                        let z = add(x, y);
                        println!("Result: {}", z);
                    }

                    fn add(a: i32, b: i32) -> i32 {
                        a + b
                    }
                "#},
                "b.rs": indoc! {"
                    pub struct Config {
                        pub name: String,
                        pub value: i32,
                    }

                    impl Config {
                        pub fn new(name: String, value: i32) -> Self {
                            Config { name, value }
                        }
                    }
                "},
                "c.rs": indoc! {r#"
                    use std::collections::HashMap;

                    fn main() {
                        let args: Vec<String> = std::env::args().collect();
                        let data: Vec<i32> = args[1..]
                            .iter()
                            .filter_map(|s| s.parse().ok())
                            .collect();
                        let result = process_data(data);
                        println!("{:?}", result);
                    }

                    fn process_data(data: Vec<i32>) -> HashMap<i32, usize> {
                        let mut counts = HashMap::new();
                        for value in data {
                            *counts.entry(value).or_insert(0) += 1;
                        }
                        counts
                    }

                    #[cfg(test)]
                    mod tests {
                        use super::*;

                        #[test]
                        fn test_process_data() {
                            let data = vec![1, 2, 2, 3];
                            let result = process_data(data);
                            assert_eq!(result.get(&2), Some(&2));
                        }
                    }
                "#}
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        let language_registry = project.read_with(cx, |project, _| project.languages().clone());
        let lang = rust_lang();
        let lang_id = lang.id();
        language_registry.add(Arc::new(lang));

        let index = cx.new(|cx| SyntaxIndex::new(&project, cx));
        cx.run_until_parked();

        (project, index, lang_id)
    }

    fn rust_lang() -> Language {
        Language::new(
            LanguageConfig {
                name: "Rust".into(),
                matcher: LanguageMatcher {
                    path_suffixes: vec!["rs".to_string()],
                    ..Default::default()
                },
                ..Default::default()
            },
            Some(tree_sitter_rust::LANGUAGE.into()),
        )
        .with_highlights_query(include_str!("../../languages/src/rust/highlights.scm"))
        .unwrap()
        .with_outline_query(include_str!("../../languages/src/rust/outline.scm"))
        .unwrap()
    }
}
