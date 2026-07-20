//! Wraps markup text

use crate::pretty::markup::{MarkupLine, MarkupRepr};
use crate::pretty::prelude::ArenaDoc;
use crate::pretty::text::is_enum_marker;
use crate::pretty::util::is_comment_node;
use crate::pretty::{Context, PrettyPrinter};
use icu_segmenter::{SentenceSegmenter, SentenceSegmenterBorrowed};
use prettyless::{Arena, DocAllocator};
use typst_syntax::ast::{Equation, Expr, Raw, Text};
use typst_syntax::{SyntaxKind, SyntaxNode};

impl<'a> PrettyPrinter<'a> {
    /// Wraps markup at the specified line width.
    /// With text-wrapping enabled, spaces may turn to linebreaks, and linebreaks may turn to spaces, if safe.
    pub(crate) fn wrap_markup_fill(&'a self, ctx: Context, repr: &MarkupRepr<'a>) -> ArenaDoc<'a> {
        let mut doc = self.arena.nil();
        for (i, line) in repr.lines.iter().enumerate() {
            let &MarkupLine {
                ref nodes, breaks, ..
            } = line;
            for (j, node) in nodes.iter().enumerate() {
                doc += if node.kind() == SyntaxKind::Space {
                    if nodes.get(j + 1).is_some_and(cannot_break_before) {
                        self.arena.space()
                    } else if nodes.get(j + 1).is_some_and(prefer_exclusive)
                        || nodes.get(j - 1).is_some_and(prefer_exclusive)
                    {
                        self.arena.hardline()
                    } else {
                        self.arena.softline()
                    }
                } else if let Some(text) = node.cast::<Text>() {
                    self.convert_text_wrapped(text)
                } else if let Some(expr) = node.cast::<Expr>() {
                    self.convert_expr(ctx, expr)
                } else if is_comment_node(node) {
                    self.convert_comment(ctx, node)
                } else {
                    // can be Hash, Semicolon, Shebang
                    self.convert_trivia_untyped(node)
                };
            }
            // Should not eat trailing parbreaks.
            if breaks == 1
                && i + 1 != repr.lines.len()
                && !nodes
                    .last()
                    .is_some_and(|last| should_break_after(last) || preserve_break_after(last))
                && !preserve_exclusive(line)
                && !preserve_exclusive(&repr.lines[i + 1])
            {
                doc += self.arena.softline();
            } else if breaks > 0 {
                doc += self.arena.hardline().repeat(breaks);
            }
        }
        doc
    }

    /// With sentence-per-line mode, split sentence boundaries inside text leaves.
    pub(crate) fn wrap_markup_sentence(
        &'a self,
        ctx: Context,
        repr: &MarkupRepr<'a>,
    ) -> ArenaDoc<'a> {
        let segmenter = SentenceSegmenter::new(Default::default());
        let mut doc = self.arena.nil();
        let mut pending_sentence_break = false;
        for line in repr.lines.iter() {
            for node in line.nodes.iter() {
                doc += if node.kind() == SyntaxKind::Space {
                    if pending_sentence_break {
                        pending_sentence_break = false;
                        self.arena.hardline()
                    } else {
                        self.arena.space()
                    }
                } else if let Some(text) = node.cast::<Text>() {
                    let (text_doc, ended_sentence) =
                        convert_text_sentence_per_line(&self.arena, &segmenter, text);
                    let leading_break = if pending_sentence_break {
                        self.arena.hardline()
                    } else {
                        self.arena.nil()
                    };
                    pending_sentence_break = ended_sentence;
                    leading_break + text_doc
                } else if let Some(expr) = node.cast::<Expr>() {
                    pending_sentence_break = source_ends_with_sentence(node.leaf_text());
                    self.convert_expr(ctx, expr)
                } else if is_comment_node(node) {
                    pending_sentence_break = false;
                    self.convert_comment(ctx, node)
                } else {
                    pending_sentence_break = source_ends_with_sentence(node.leaf_text());
                    self.convert_trivia_untyped(node)
                };
            }
            if line.breaks > 0 {
                doc += self.arena.hardline().repeat(line.breaks);
                pending_sentence_break = false;
            }
        }

        doc
    }
}

/// For NOT space -> soft-line: \
/// Ensure special markup characters are not misinterpreted as markup markers after reflow.
///
/// Besides, reflowing labels to the next line is not desired.
fn cannot_break_before(node: &&SyntaxNode) -> bool {
    let text = node.leaf_text();
    matches!(text.as_str(), "=" | "+" | "-" | "/")
        || matches!(node.kind(), SyntaxKind::Label)
        || is_enum_marker(text)
}

/// For space -> hard-line: \
/// Prefers block equations exclusive to a single line.
fn prefer_exclusive(node: &&SyntaxNode) -> bool {
    is_block_equation(node) || is_block_raw(node)
}

/// For NOT hard-line -> soft-line: \
/// Should always break after block elements or line comments.
fn should_break_after(node: &SyntaxNode) -> bool {
    matches!(
        node.kind(),
        SyntaxKind::Heading
            | SyntaxKind::ListItem
            | SyntaxKind::EnumItem
            | SyntaxKind::TermItem
            | SyntaxKind::LineComment
    )
}

/// For NOT hard-line -> soft-line: \
/// Breaking after them is visually better.
fn preserve_break_after(node: &SyntaxNode) -> bool {
    matches!(
        node.kind(),
        SyntaxKind::BlockComment
            | SyntaxKind::Linebreak
            | SyntaxKind::Label
            | SyntaxKind::CodeBlock
            | SyntaxKind::ContentBlock
            | SyntaxKind::Conditional
            | SyntaxKind::WhileLoop
            | SyntaxKind::ForLoop
            | SyntaxKind::Contextual
    ) || is_block_equation(node)
        || is_block_raw(node)
}

/// For NOT hard-line -> soft-line: \
/// Keeps the line exclusive (prevents soft breaks) when:
/// - It contains only one non-text node, or
/// - It contains exactly two nodes where the first is a Hash, such as `#figure()`.
fn preserve_exclusive(line: &MarkupLine) -> bool {
    let nodes = &line.nodes;
    let len = nodes.len();
    len == 1 && nodes[0].kind() != SyntaxKind::Text
        || len == 2 && nodes[0].kind() == SyntaxKind::Hash
        || len > 0 && prefer_exclusive(&nodes[0])
}

fn is_block_equation(it: &SyntaxNode) -> bool {
    it.cast::<Equation>()
        .is_some_and(|equation| equation.block())
}

fn is_block_raw(it: &SyntaxNode) -> bool {
    it.cast::<Raw>().is_some_and(|raw| raw.block())
}

fn convert_text_sentence_per_line<'a>(
    arena: &'a Arena<'a>,
    segmenter: &SentenceSegmenterBorrowed,
    text: Text<'a>,
) -> (ArenaDoc<'a>, bool) {
    let text = text.get();
    let mut boundaries = segmenter.segment_str(text);
    let Some(mut start) = boundaries.next() else {
        return (arena.nil(), false);
    };
    let mut doc = arena.nil();
    let mut first = true;
    let mut ended_sentence = false;
    let mut previous_was_abbreviation = false;

    for end in boundaries {
        let sentence = text[start..end].trim();
        if !sentence.is_empty() {
            if !first {
                doc += if previous_was_abbreviation {
                    arena.space()
                } else {
                    arena.hardline()
                };
            }
            doc += arena.text(sentence);
            if end == text.len() && text.ends_with(' ') {
                doc += arena.space();
            }
            first = false;
            previous_was_abbreviation = is_common_abbreviation(sentence);
            ended_sentence = sentence_ends_with_punctuation(sentence) && !previous_was_abbreviation;
        }
        start = end;
    }

    (doc, ended_sentence)
}

fn sentence_ends_with_punctuation(text: &str) -> bool {
    text.ends_with(['.', '!', '?', '。', '！', '？'])
}

fn is_common_abbreviation(text: &str) -> bool {
    matches!(
        text.to_ascii_lowercase().as_str(),
        "dr."
            | "mr."
            | "mrs."
            | "ms."
            | "prof."
            | "sr."
            | "jr."
            | "st."
            | "vs."
            | "etc."
            | "e.g."
            | "i.e."
    )
}

fn source_ends_with_sentence(text: &str) -> bool {
    let trimmed = text.trim_end_matches([' ', '\t', '\n', '\r', ')', ']', '}']);
    sentence_ends_with_punctuation(trimmed)
}
