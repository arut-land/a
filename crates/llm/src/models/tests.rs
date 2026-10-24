use super::openai::*;
use crate::Effort;

#[test]
fn ids_name_what_models_do() {
    assert_eq!(gpt_version("openai/gpt-5.5-pro"), Some((5, 5)));
    assert_eq!(gpt_version("gpt-6-luna"), Some((6, 0)));
    assert_eq!(gpt_version("gpt-4.x"), Some((4, 0)));
    assert_eq!(gpt_version("gpt-x"), None);
    assert_eq!(reasons("gpt-6-luna"), Some(true));
    assert_eq!(reasons("o4-mini"), Some(true));
    assert_eq!(reasons("gpt-4.1"), Some(false));
    assert_eq!(reasons("my-deployment"), None);
    assert!(responses_can_disable("gpt-6-luna"));
    assert!(!responses_can_disable("gpt-6-astra"));
    assert!(!responses_can_disable("o3"));
    assert!(sampling("gpt-4o").takes(None));
    assert!(!sampling("gpt-6-luna").takes(None));
    assert!(sampling("gpt-6-luna").takes(Some(Effort::None)));
    assert!(!sampling("gpt-5.5").takes(Some(Effort::None)));
    assert!(!reads_images("o3-mini"));
    assert!(reads_images("gpt-6-luna"));
}

#[test]
fn a_window_is_known_only_for_ids_a_row_names() {
    use super::context_window;
    assert_eq!(context_window("anthropic/claude-sonnet-4-5"), Some(200_000));
    assert_eq!(context_window("gpt-5-chat-latest"), Some(128_000));
    assert_eq!(context_window("gpt-5-mini"), Some(400_000));
    assert_eq!(context_window("gemini-1.5-pro-002"), Some(2_097_152));
    assert_eq!(context_window("o1-preview-2024-09-12"), Some(128_000));
    assert_eq!(context_window("gpt-5.1-chat-latest"), Some(128_000));
    assert_eq!(context_window("grok-4-fast-reasoning"), Some(2_000_000));
    assert_eq!(context_window("x-ai/grok-4-1-fast"), Some(2_000_000));
    assert_eq!(context_window("grok-4-0709"), Some(256_000));
    assert_eq!(
        context_window("us.anthropic.claude-sonnet-4-5-20250929-v1:0"),
        Some(200_000)
    );
    assert_eq!(context_window("my-deployment"), None);
}
