#[test]
fn citations_resolve_against_their_own_grounding_block() {
    let part = |url: &str, claim: &str| crate::history::Part {
        content: crate::history::Content::Text("".into()),
        replay: Some(std::sync::Arc::new(
            serde_json::json!({"groundingMetadata": {
                "groundingChunks": [{"web": {"uri": url, "title": url}}],
                "groundingSupports": [{"segment": {"text": claim}, "groundingChunkIndices": [0]}],
            }}),
        )),
    };
    let mut result = super::super::search::SearchResult::default();
    super::collect_search(
        &[
            part("https://a.example", "claim A"),
            part("https://b.example", "claim B"),
        ],
        &mut result,
    );
    let claims: Vec<_> = result
        .sources
        .iter()
        .map(|source| (source.url.as_str(), source.claims.clone()))
        .collect();
    assert_eq!(
        claims,
        [
            ("https://a.example", vec!["claim A".to_owned()]),
            ("https://b.example", vec!["claim B".to_owned()]),
        ]
    );
}

#[test]
fn empty_notification_does_not_emit_empty_part() {
    use crate::{
        history::{Message, Notification, NotificationKind},
        provider::{HttpProtocol, Kind, Provider, Request, Tools},
    };
    let provider = Provider {
        kind: Kind::Custom {
            name: "gemini".into(),
            protocol: HttpProtocol::Gemini,
            endpoint: Box::new(
                reqwest::Url::parse("https://generativelanguage.googleapis.com/v1beta/models")
                    .unwrap(),
            ),
            key: true,
        },
    };
    let history = [
        Message::Notification(Notification {
            text: "".into(),
            kind: NotificationKind::Recovery,
        }),
        Message::User("hi".into()),
    ];
    let request = Request {
        provider: &provider,
        model: "gemini-2.5-pro",
        instructions: "",
        history: &history,
        tools: Tools::Functions(&[]),
        reasoning_effort: None,
        summaries: false,
        verbosity: None,
        max_output_tokens: None,
        cache: None,
    };
    let body = super::body(&request).unwrap();
    let parts = body["contents"][0]["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 1, "unexpected empty part: {body}");
    assert_eq!(parts[0]["text"], "hi");
}
