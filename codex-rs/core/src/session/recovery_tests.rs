use super::INVALID_IMAGE_RETRY_PLACEHOLDER;
use super::MAX_OUTPUT_RESUME_ATTEMPTS;
use super::RecoveryState;
use super::omit_images_for_recovery;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ImageReference;
use codex_protocol::models::ResponseItem;

fn inline_image() -> ImageReference {
    ImageReference::Inline {
        image_url: "data:image/png;base64,AAAA".to_string(),
    }
}

#[test]
fn recovery_projection_removes_images_from_messages_and_tool_outputs() {
    let mut items = vec![
        ResponseItem::Message {
            status: None,
            id: None,
            role: "user".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "inspect this".to_string(),
                },
                ContentItem::InputImage {
                    image: inline_image(),
                    detail: None,
                },
            ],
            phase: None,
            encrypted_content: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: None,
            name: None,
            namespace: None,
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::ContentItems(vec![
                    FunctionCallOutputContentItem::InputImage {
                        image: inline_image(),
                        detail: None,
                    },
                    FunctionCallOutputContentItem::InputText {
                        text: "kept".to_string(),
                    },
                ]),
                success: None,
            },
            internal_chat_message_metadata_passthrough: None,
        },
    ];

    assert_eq!(omit_images_for_recovery(&mut items), 2);
    assert_eq!(
        serde_json::to_value(&items).expect("serialize recovered items"),
        serde_json::json!([
            {
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "inspect this"},
                    {"type": "input_text", "text": INVALID_IMAGE_RETRY_PLACEHOLDER}
                ]
            },
            {
                "type": "function_call_output",
                "output": [
                    {"type": "input_text", "text": INVALID_IMAGE_RETRY_PLACEHOLDER},
                    {"type": "input_text", "text": "kept"}
                ]
            }
        ])
    );
}

#[test]
fn recovery_state_caps_each_one_shot_path() {
    let mut recovery = RecoveryState::default();

    assert!(recovery.claim_reactive_compact());
    assert!(!recovery.claim_reactive_compact());
    assert!(recovery.claim_invalid_image_retry());
    assert!(!recovery.claim_invalid_image_retry());

    for expected in 1..=MAX_OUTPUT_RESUME_ATTEMPTS {
        assert_eq!(recovery.claim_max_output_resume(), Some(expected));
    }
    assert_eq!(recovery.claim_max_output_resume(), None);

    recovery.record_success();
    assert!(recovery.claim_reactive_compact());
    assert!(!recovery.claim_invalid_image_retry());
    assert!(!recovery.can_attempt_max_output_resume());
}
