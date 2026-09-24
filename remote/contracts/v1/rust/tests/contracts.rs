#[path = "../src/fixture_support.rs"]
mod fixture_support;

#[test]
fn shared_wire_fixtures_preserve_identity_and_reject_invalid_boundaries() {
    let results = fixture_support::run();
    assert!(results.iter().any(|r| r["valid"] == true));
    assert!(results.iter().any(|r| r["valid"] == false));
}

#[test]
fn producer_revalidates_programmatically_changed_contracts() {
    use rsi_remote_contracts_v1::{WireDocumentV1, decode, encode};
    let mut d = decode(br#"{"type":"request","value":{"method":"RemoteListSessionsV1","params":{"project_id":"00000000-0000-4000-8000-000000000001"}}}"#).unwrap();
    let WireDocumentV1::Request(rsi_remote_contracts_v1::ReadRequestV1::RemoteListSessionsV1(q)) =
        &mut d
    else {
        panic!("request")
    };
    q.limit = 101;
    assert!(encode(&d).is_err());
}

#[test]
fn correction_review_controls_preserve_retained_questions_and_durable_publication_provenance() {
    use rsi_remote_contracts_v1::{decode, encode};
    use serde_json::{Value, json};
    let corpus: Value = serde_json::from_str(include_str!("../../fixtures/corpus.json")).unwrap();
    let fixture = |id: &str| {
        corpus["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == id)
            .unwrap()["input"]
            .clone()
    };
    let selection = fixture("create-none");
    let retained = fixture("r008-generic-tombstone");
    let publication = fixture("generic-multiple-questions-options");
    let mut selection_array = selection.clone();
    selection_array["value"]["selection"] = json!(["none"]);
    let mut retained_array = retained.clone();
    let display = &retained["value"]["result"]["selected"]["last_display"];
    retained_array["value"]["result"]["selected"]["last_display"] = json!([
        "generic_questions",
        display["questions"],
        display["omitted_questions"],
        display["details_state"]
    ]);
    let mut mirror = publication.clone();
    let mut source = publication["value"]["result"]["items"][0]["source_observations"][0].clone();
    source["source"] = json!("durable_question_fallback");
    mirror["value"]["result"]["items"][0]["source_observations"]
        .as_array_mut()
        .unwrap()
        .push(source);
    let inputs = [
        selection,
        selection_array,
        retained,
        retained_array,
        publication,
        mirror,
    ];
    let expected = [true, false, true, false, true, true];
    let mut outputs = Vec::new();
    for (input, valid) in inputs.iter().zip(expected) {
        let result = decode(&serde_json::to_vec(input).unwrap());
        assert_eq!(result.is_ok(), valid);
        if let Ok(document) = result {
            let output: Value = serde_json::from_slice(&encode(&document).unwrap()).unwrap();
            assert_eq!(&output, input);
            outputs.push(output);
        } else {
            outputs.push(Value::Null);
        }
    }
    let questions = json!([
        {"header":"Checks","question":"Which checks?","options":[{"label":"Unit","description":"Fast tests"},{"label":"Browser","description":"Browser interactions"}],"multi_select":true,"omitted_options":"0"},
        {"header":"Branch","question":"Which branch?","options":[{"label":"Current branch","description":"Keep sandbox custody"}],"multi_select":false,"omitted_options":"0"}
    ]);
    assert_eq!(
        outputs[2]["value"]["result"]["selected"]["last_display"]["questions"],
        questions
    );
    for n in [4, 5] {
        let card = &outputs[n]["value"]["result"]["items"][0];
        assert_eq!(card["id"], "question:00000000-0000-4000-8000-000000000050");
        assert_eq!(card["identity_class"], "publication");
        assert_eq!(card["questions"], questions);
        assert_eq!(card["details_state"], "complete");
        assert_eq!(card["omitted_questions"], "0");
        assert_eq!(card["omitted_source_observations"], "0");
        assert_eq!(card["disagreement"], false);
    }
    let sources = outputs[5]["value"]["result"]["items"][0]["source_observations"]
        .as_array()
        .unwrap();
    assert_eq!(
        sources
            .iter()
            .map(|s| s["source"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["question_publications", "durable_question_fallback"]
    );
    assert!(sources.iter().all(|s| s["state"] == "complete"));
}
