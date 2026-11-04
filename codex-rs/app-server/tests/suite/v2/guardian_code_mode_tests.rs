//! Verifies transparent model-policy wrappers and unchanged legacy Code Mode behavior.

use super::*;
use codex_protocol::openai_models::GuardianModelPolicy;
use codex_protocol::openai_models::GuardianReviewMode;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[derive(Clone, Copy)]
enum PolicyConfig {
    Model,
    RequiredModel,
    RequiredLegacy,
    Legacy,
    LegacyDisabled,
}
