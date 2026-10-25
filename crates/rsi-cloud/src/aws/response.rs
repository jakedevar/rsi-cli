//! Only the response fields consumed by cloud-gate/cloud-pool. Required
//! containers/ids/state names prevent a malformed success from meaning empty.
use super::{Error, Result};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
pub(super) struct Items<T> {
    #[serde(default)]
    pub item: Vec<T>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Describe {
    pub reservation_set: Items<Reservation>,
    pub next_token: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Reservation {
    pub instances_set: Items<Instance>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Instance {
    pub instance_id: String,
    pub instance_state: State,
    pub state_reason: Option<Reason>,
}
#[derive(Deserialize)]
pub(super) struct State {
    pub name: String,
}
#[derive(Deserialize)]
pub(super) struct Reason {
    pub code: String,
    pub message: Option<String>,
}
#[derive(Deserialize)]
pub(super) struct Console {
    pub output: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Terminate {
    pub instances_set: Items<Terminated>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Terminated {
    pub instance_id: String,
    pub current_state: State,
    pub previous_state: State,
}

pub(super) fn parse<T: serde::de::DeserializeOwned>(body: &str, root: &str) -> Result<T> {
    let mut reader = quick_xml::Reader::from_str(body);
    // Serde itself does not enforce the root name. Explicitly reject error/HTML
    // roots even with HTTP 200, and reject trailing/multiple documents.
    let mut depth = 0_u32;
    let mut seen = false;
    loop {
        use quick_xml::events::Event;
        match reader
            .read_event()
            .map_err(|_| Error("invalid AWS XML response"))?
        {
            Event::Start(e) if depth == 0 => {
                if seen || e.name().as_ref() != root.as_bytes() {
                    return Err(Error("unexpected AWS response root"));
                }
                seen = true;
                depth += 1;
            }
            Event::Empty(e) if depth == 0 => {
                if seen || e.name().as_ref() != root.as_bytes() {
                    return Err(Error("unexpected AWS response root"));
                }
                seen = true;
            }
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                depth = depth
                    .checked_sub(1)
                    .ok_or(Error("invalid AWS XML response"))?
            }
            Event::DocType(_) => return Err(Error("unsupported AWS XML response")),
            Event::Text(e) if depth == 0 && e.iter().any(|b| !b.is_ascii_whitespace()) => {
                return Err(Error("invalid AWS XML response"));
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !seen || depth != 0 {
        return Err(Error("incomplete AWS XML response"));
    }
    quick_xml::de::from_str(body).map_err(|_| Error("invalid AWS response fields"))
}

pub(super) fn instance_json(instance: Instance) -> Result<Value> {
    if !super::valid_id(&instance.instance_id) || instance.instance_state.name.is_empty() {
        return Err(Error("invalid AWS instance fields"));
    }
    let mut value = json!({"InstanceId": instance.instance_id, "State": {"Name": instance.instance_state.name}});
    if let Some(reason) = instance.state_reason {
        value["StateReason"] =
            json!({"Code": reason.code, "Message": reason.message.unwrap_or_default()});
    }
    Ok(value)
}
