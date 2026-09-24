use rsi_remote_contracts_v1::{
    WireDocumentV1, decode, display_hints, encode, validate_bound_read, validate_bound_response,
    validate_exchange,
};
use serde_json::{Value, json};

fn case<'a>(cases: &'a [Value], id: &str) -> &'a Value {
    cases
        .iter()
        .find(|v| v["id"] == id)
        .expect("fixture reference")
}
fn input(cases: &[Value], c: &Value) -> Value {
    if let Some(v) = c.get("input") {
        return v.clone();
    }
    let mut v = input(
        cases,
        case(cases, c["base"].as_str().expect("fixture base")),
    );
    for patch in c["patches"].as_array().expect("patch list") {
        let parts: Vec<_> = patch["path"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap())
            .collect();
        let path = if parts.len() == 1 {
            String::new()
        } else {
            format!("/{}", parts[..parts.len() - 1].join("/"))
        };
        let parent = v.pointer_mut(&path).expect("patch parent");
        let key = parts.last().unwrap();
        if let Some(array) = parent.as_array_mut() {
            let i: usize = key.parse().unwrap();
            if patch["op"] == "remove" {
                array.remove(i);
            } else {
                array[i] = patch["value"].clone();
            }
        } else {
            let object = parent.as_object_mut().unwrap();
            if patch["op"] == "remove" {
                assert!(object.remove(*key).is_some());
            } else {
                object.insert((*key).into(), patch["value"].clone());
            }
        }
    }
    v
}
fn bytes(cases: &[Value], c: &Value) -> Vec<u8> {
    if let Some(hex) = c.get("raw_hex").and_then(Value::as_str) {
        return hex
            .as_bytes()
            .chunks_exact(2)
            .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
            .collect();
    }
    c.get("raw").map_or_else(
        || serde_json::to_vec(&input(cases, c)).unwrap(),
        |r| r.as_str().unwrap().as_bytes().to_vec(),
    )
}
fn qualification(cases: &[Value], c: &Value) -> Result<WireDocumentV1, Box<dyn std::error::Error>> {
    let d = decode(&bytes(cases, c))?;
    if let Some(id) = c.get("against").and_then(Value::as_str) {
        let q = decode(&bytes(cases, case(cases, id)))?;
        match (&q, &d) {
            (WireDocumentV1::Request(q), WireDocumentV1::Response(r)) => validate_exchange(q, r)?,
            (WireDocumentV1::BoundRead(q), WireDocumentV1::BoundResponse(r)) => {
                validate_bound_response(q, r)?
            }
            _ => panic!("invalid fixture correlation"),
        }
    }
    if let Some(id) = c.get("view").and_then(Value::as_str) {
        let view = decode(&bytes(cases, case(cases, id)))?;
        match (&view, &d) {
            (WireDocumentV1::ViewState(view), WireDocumentV1::BoundRead(q)) => {
                validate_bound_read(view, q)?
            }
            _ => panic!("invalid fixture view"),
        }
    }
    Ok(d)
}
pub fn run() -> Vec<Value> {
    let corpus: Value = serde_json::from_str(include_str!("../../fixtures/corpus.json")).unwrap();
    assert_eq!(corpus["schema_version"], 1);
    let cases = corpus["cases"].as_array().unwrap();
    let mut ids = std::collections::BTreeSet::new();
    let mut results = Vec::new();
    for c in cases {
        let id = c["id"].as_str().unwrap();
        assert!(ids.insert(id), "duplicate fixture ID");
        let result = qualification(cases, c);
        assert_eq!(
            result.is_ok(),
            c["valid"].as_bool().unwrap(),
            "fixture {id}: {result:?}"
        );
        if let Ok(d) = result {
            let actual: Value =
                serde_json::from_slice(&encode(&d).expect("valid producer")).unwrap();
            let expected = c
                .get("expected")
                .cloned()
                .unwrap_or_else(|| input(cases, c));
            assert_eq!(actual, expected, "canonical fixture {id}");
            assert_eq!(decode(&encode(&d).unwrap()).unwrap(), d, "round trip {id}");
            let hints = display_hints(&d).unwrap();
            if let Some(originals) = c.get("source_tool_ids").and_then(Value::as_array) {
                assert_ne!(originals[0], originals[1]);
                for original in originals {
                    assert!(original.as_str().unwrap().len() > 256);
                }
                assert_eq!(
                    &originals[0].as_str().unwrap()[..256],
                    &originals[1].as_str().unwrap()[..256]
                );
            }
            for text in c["visible"].as_array().unwrap() {
                assert!(
                    hints.iter().any(|h| h == text.as_str().unwrap()),
                    "positive display identity {id}: {text}"
                );
            }
            results.push(json!({"id":id,"valid":true,"value":actual,"hints":hints}));
        } else {
            results.push(json!({"id":id,"valid":false}));
        }
    }
    results
}
