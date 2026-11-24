use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::input_text;

pub(crate) const MAX_OUTPUT_RESUME_ATTEMPTS: u8 = 3;

/// One-shot recovery claims for the current turn.
///
/// Keep these flags together so a future branch cannot reset only part of the state and
/// accidentally reopen a recovery loop.
#[derive(Debug, Default)]
pub(crate) struct RecoveryState {
    reactive_compact_attempted: bool,
    max_output_resume_attempts: u8,
    invalid_image_retry_attempted: bool,
}

impl RecoveryState {
    pub(crate) fn record_success(&mut self) {
        self.reactive_compact_attempted = false;
    }

    pub(crate) fn can_attempt_reactive_compact(&self) -> bool {
        !self.reactive_compact_attempted
    }

    #[must_use]
    pub(crate) fn claim_reactive_compact(&mut self) -> bool {
        if !self.can_attempt_reactive_compact() {
            return false;
        }
        self.reactive_compact_attempted = true;
        true
    }

    pub(crate) fn can_attempt_max_output_resume(&self) -> bool {
        self.max_output_resume_attempts < MAX_OUTPUT_RESUME_ATTEMPTS
    }

    #[must_use]
    pub(crate) fn claim_max_output_resume(&mut self) -> Option<u8> {
        if !self.can_attempt_max_output_resume() {
            return None;
        }
        self.max_output_resume_attempts = self.max_output_resume_attempts.saturating_add(1);
        Some(self.max_output_resume_attempts)
    }

    pub(crate) fn can_attempt_invalid_image_retry(&self) -> bool {
        !self.invalid_image_retry_attempted
    }

    #[must_use]
    pub(crate) fn claim_invalid_image_retry(&mut self) -> bool {
        if !self.can_attempt_invalid_image_retry() {
            return false;
        }
        self.invalid_image_retry_attempted = true;
        true
    }
}

/// Set after an invalid-image response so later attempts in the same turn omit images
/// from their request projection while leaving the recorded history untouched.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct OmitImagesForRecovery;

pub(crate) const INVALID_IMAGE_RETRY_PLACEHOLDER: &str = "image content omitted after the model rejected it as invalid; continue using the surrounding text or ask for a smaller replacement image";

pub(crate) fn omit_images_for_recovery(items: &mut [ResponseItem]) -> usize {
    let mut omitted = 0;
    for item in items {
        match item {
            ResponseItem::Message { content, .. } => {
                for content_item in content {
                    if matches!(content_item, ContentItem::InputImage { .. }) {
                        *content_item = input_text(INVALID_IMAGE_RETRY_PLACEHOLDER);
                        omitted += 1;
                    }
                }
            }
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                let FunctionCallOutputBody::ContentItems(content_items) = &mut output.body else {
                    continue;
                };
                for content_item in content_items {
                    if matches!(
                        content_item,
                        FunctionCallOutputContentItem::InputImage { .. }
                    ) {
                        *content_item = FunctionCallOutputContentItem::InputText {
                            text: INVALID_IMAGE_RETRY_PLACEHOLDER.to_string(),
                        };
                        omitted += 1;
                    }
                }
            }
            _ => {}
        }
    }
    omitted
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;
