use gpui::{App, WeakEntity};
use language::{Buffer, BufferSnapshot, LanguageId};
use project::ProjectEntryId;
use std::borrow::Cow;
use std::ops::{Deref, Range};
use std::sync::Arc;
use text::{Anchor, Bias, OffsetRangeExt, ToOffset};

use crate::outline::OutlineDeclaration;

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct Identifier {
    pub name: Arc<str>,
    pub language_id: LanguageId,
}

slotmap::new_key_type! {
    pub struct DeclarationId;
}

#[derive(Debug, Clone)]
pub enum Declaration {
    File {
        project_entry_id: ProjectEntryId,
        declaration: FileDeclaration,
    },
    Buffer {
        buffer: WeakEntity<Buffer>,
        declaration: BufferDeclaration,
    },
}

const ITEM_TEXT_TRUNCATION_LENGTH: usize = 1024;

impl Declaration {
    pub fn identifier(&self) -> &Identifier {
        match self {
            Declaration::File { declaration, .. } => &declaration.identifier,
            Declaration::Buffer { declaration, .. } => &declaration.identifier,
        }
    }

    pub fn project_entry_id(&self, cx: &App) -> Option<ProjectEntryId> {
        match self {
            Declaration::File {
                project_entry_id, ..
            } => Some(*project_entry_id),
            Declaration::Buffer { buffer, .. } => buffer
                .read_with(cx, |buffer, _cx| {
                    project::File::from_dyn(buffer.file())
                        .and_then(|file| file.project_entry_id(cx))
                })
                .ok()
                .flatten(),
        }
    }

    pub fn item_text(&self, cx: &App) -> (Cow<'_, str>, bool) {
        match self {
            Declaration::File { declaration, .. } => (
                declaration.text.as_ref().into(),
                declaration.text_is_truncated,
            ),
            Declaration::Buffer {
                buffer,
                declaration,
            } => buffer
                .read_with(cx, |buffer, _cx| {
                    let (range, is_truncated) = expand_range_to_line_boundaries_and_truncate(
                        &declaration.item_range,
                        ITEM_TEXT_TRUNCATION_LENGTH,
                        buffer.deref(),
                    );
                    (
                        buffer.text_for_range(range).collect::<Cow<str>>(),
                        is_truncated,
                    )
                })
                .unwrap_or_default(),
        }
    }

    pub fn signature_text(&self, cx: &App) -> (Cow<'_, str>, bool) {
        match self {
            Declaration::File { declaration, .. } => (
                declaration.text[declaration.signature_range_in_text.clone()].into(),
                declaration.signature_is_truncated,
            ),
            Declaration::Buffer {
                buffer,
                declaration,
            } => buffer
                .read_with(cx, |buffer, _cx| {
                    let (range, is_truncated) = expand_range_to_line_boundaries_and_truncate(
                        &declaration.signature_range,
                        ITEM_TEXT_TRUNCATION_LENGTH,
                        buffer.deref(),
                    );
                    (
                        buffer.text_for_range(range).collect::<Cow<str>>(),
                        is_truncated,
                    )
                })
                .unwrap_or_default(),
        }
    }
}

fn expand_range_to_line_boundaries_and_truncate<T: ToOffset>(
    range: &Range<T>,
    limit: usize,
    buffer: &text::BufferSnapshot,
) -> (Range<usize>, bool) {
    let mut point_range = range.to_point(buffer);
    point_range.start.column = 0;
    point_range.end.row += 1;
    point_range.end.column = 0;

    let mut item_range = point_range.to_offset(buffer);
    let is_truncated = item_range.len() > limit;
    if is_truncated {
        item_range.end = item_range.start + limit;
    }
    item_range.end = buffer.clip_offset(item_range.end, Bias::Left);
    (item_range, is_truncated)
}

#[derive(Debug, Clone)]
pub struct FileDeclaration {
    pub parent: Option<DeclarationId>,
    pub identifier: Identifier,
    /// offset range of the declaration in the file, expanded to line boundaries and truncated
    pub item_range_in_file: Range<usize>,
    /// text of `item_range_in_file`
    pub text: Arc<str>,
    /// whether `text` was truncated
    pub text_is_truncated: bool,
    /// offset range of the signature within `text`
    pub signature_range_in_text: Range<usize>,
    /// whether `signature` was truncated
    pub signature_is_truncated: bool,
}

impl FileDeclaration {
    pub fn from_outline(
        declaration: OutlineDeclaration,
        snapshot: &BufferSnapshot,
    ) -> FileDeclaration {
        let (item_range_in_file, text_is_truncated) = expand_range_to_line_boundaries_and_truncate(
            &declaration.item_range,
            ITEM_TEXT_TRUNCATION_LENGTH,
            snapshot,
        );

        // TODO: consider logging if unexpected
        let signature_start = declaration
            .signature_range
            .start
            .saturating_sub(item_range_in_file.start);
        let mut signature_end = declaration
            .signature_range
            .end
            .saturating_sub(item_range_in_file.start);
        let signature_is_truncated = signature_end > item_range_in_file.len();
        if signature_is_truncated {
            signature_end = item_range_in_file.len();
        }

        FileDeclaration {
            parent: None,
            identifier: declaration.identifier,
            signature_range_in_text: signature_start..signature_end,
            signature_is_truncated,
            text: snapshot
                .text_for_range(item_range_in_file.clone())
                .collect::<String>()
                .into(),
            text_is_truncated,
            item_range_in_file,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BufferDeclaration {
    pub parent: Option<DeclarationId>,
    pub identifier: Identifier,
    pub item_range: Range<Anchor>,
    pub signature_range: Range<Anchor>,
}

impl BufferDeclaration {
    pub fn from_outline(declaration: OutlineDeclaration, snapshot: &BufferSnapshot) -> Self {
        // use of anchor_before is a guess that the proper behavior is to expand to include
        // insertions immediately before the declaration, but not for insertions immediately after
        Self {
            parent: None,
            identifier: declaration.identifier,
            item_range: snapshot.anchor_before(declaration.item_range.start)
                ..snapshot.anchor_before(declaration.item_range.end),
            signature_range: snapshot.anchor_before(declaration.signature_range.start)
                ..snapshot.anchor_before(declaration.signature_range.end),
        }
    }
}
