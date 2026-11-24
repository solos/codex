//! One-shot instruction used after an output-token truncation.

use super::ContextualUserFragment;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::input_text;

pub(crate) struct ResumeDirectlyGuidance;

impl ContextualUserFragment for ResumeDirectlyGuidance {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("generic.resume_directly".to_string())
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<resume_directly>", "</resume_directly>")
    }

    fn body(&self) -> ContentItem {
        input_text(
            "\nThe previous response ended because it reached the output-token limit. \
             Resume directly from where it stopped. Do not apologize, recap, or repeat completed work.\n"
                .to_string(),
        )
    }
}
