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
