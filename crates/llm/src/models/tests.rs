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
