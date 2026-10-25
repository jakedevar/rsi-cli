//! Minimal AWS CLI-compatible gate/pool API surface (#3952). Fixed HTTPS
//! endpoints, no redirects or endpoint overrides, bounded responses/timeouts.
//! Diagnostics are static: neither transport errors nor AWS bodies/credentials
//! are formatted into stderr. See README.md for scope and dependency choices.
mod credentials;
mod response;
mod sign;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

use crate::args::{self, Kind};
use base64::Engine;
use serde_json::{Value, json};

#[derive(Debug)]
struct Error(&'static str);
type Result<T> = std::result::Result<T, Error>;

enum Operation {
    Describe(Vec<(String, String)>),
    Terminate(Vec<(String, String)>),
    Console(Vec<(String, String)>),
    Head(String),
}
struct Command {
    operation: Operation,
    profile: Option<String>,
    region: Option<String>,
    text: bool,
}

fn valid_id(id: &str) -> bool {
    id.starts_with("i-")
        && id.len() > 2
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn command(argv: &[String]) -> Result<Command> {
    let parsed = args::parse(
        argv,
        &[
            ("--region", Kind::Value),
            ("--profile", Kind::Value),
            ("--output", Kind::Value),
            ("--query", Kind::Value),
            ("--instance-ids", Kind::Multi),
            ("--instance-id", Kind::Value),
            ("--filters", Kind::Multi),
            ("--latest", Kind::Flag),
            ("--bucket", Kind::Value),
        ],
    )
    .map_err(|_| Error("invalid AWS command arguments"))?;
    let words: Vec<&str> = parsed.positionals.iter().map(String::as_str).collect();
    let ids = parsed.multi("--instance-ids");
    if ids.iter().any(|id| !valid_id(id)) {
        return Err(Error("invalid instance id"));
    }
    let id_params = || {
        ids.iter()
            .enumerate()
            .map(|(i, id)| (format!("InstanceId.{}", i + 1), id.clone()))
            .collect::<Vec<_>>()
    };
    let operation = match words.as_slice() {
        ["ec2", "describe-instances"] => {
            let mut params = id_params();
            let filters = parsed.multi("--filters");
            if ids.is_empty() && filters.is_empty() {
                return Err(Error("instance ids or filters required"));
            }
            if !ids.is_empty() && !filters.is_empty() {
                return Err(Error("choose ids or filters"));
            }
            for (i, filter) in filters.iter().enumerate() {
                let (name, values) = filter
                    .strip_prefix("Name=")
                    .and_then(|f| f.split_once(",Values="))
                    .ok_or(Error("invalid instance filter"))?;
                if !matches!(name, "tag:Service" | "instance-state-name") || values.is_empty() {
                    return Err(Error("unsupported instance filter"));
                }
                params.push((format!("Filter.{}.Name", i + 1), name.to_owned()));
                for (j, value) in values.split(',').enumerate() {
                    if value.is_empty() {
                        return Err(Error("empty instance filter"));
                    }
                    params.push((
                        format!("Filter.{}.Value.{}", i + 1, j + 1),
                        value.to_owned(),
                    ));
                }
            }
            if ids.is_empty() {
                params.push(("MaxResults".into(), "1000".into()));
            }
            Operation::Describe(params)
        }
        ["ec2", "terminate-instances"] if !ids.is_empty() => Operation::Terminate(id_params()),
        ["ec2", "get-console-output"] => {
            let id = parsed
                .value("--instance-id")
                .filter(|id| valid_id(id))
                .ok_or(Error("instance id required"))?;
            let mut params = vec![("InstanceId".into(), id.into())];
            if parsed.flag("--latest") {
                params.push(("Latest".into(), "true".into()));
            }
            Operation::Console(params)
        }
        ["s3api", "head-bucket"] => Operation::Head(
            parsed
                .value("--bucket")
                .ok_or(Error("bucket required"))?
                .into(),
        ),
        _ => {
            return Err(Error(
                "supported: ec2 describe-instances, terminate-instances, get-console-output; s3api head-bucket",
            ));
        }
    };
    let allowed: &[&str] = match &operation {
        Operation::Describe(_) => &["--instance-ids", "--filters"],
        Operation::Terminate(_) => &["--instance-ids"],
        Operation::Console(_) => &["--instance-id", "--latest"],
        Operation::Head(_) => &["--bucket"],
    };
    for option in [
        "--instance-ids",
        "--filters",
        "--instance-id",
        "--latest",
        "--bucket",
    ] {
        if parsed.flag(option) && !allowed.contains(&option) {
            return Err(Error("option is unsupported for this AWS operation"));
        }
    }
    let text = match parsed.value("--output").unwrap_or("json") {
        "json" => false,
        "text" => true,
        _ => return Err(Error("unsupported AWS output format")),
    };
    let expected_query = match &operation {
        Operation::Describe(_) if text => Some("Reservations[].Instances[].InstanceId"),
        Operation::Console(_) if text => Some("Output"),
        _ => None,
    };
    if parsed.value("--query") != expected_query {
        return Err(Error("unsupported AWS query/output combination"));
    }
    Ok(Command {
        operation,
        profile: parsed.value("--profile").map(str::to_owned),
        region: parsed.value("--region").map(str::to_owned),
        text,
    })
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
fn form(action: &str, params: &[(String, String)]) -> String {
    std::iter::once(("Action".to_owned(), action.to_owned()))
        .chain(std::iter::once(("Version".into(), "2016-11-15".into())))
        .chain(params.iter().cloned())
        .map(|(key, value)| format!("{}={}", encode(&key), encode(&value)))
        .collect::<Vec<_>>()
        .join("&")
}

struct Client {
    http: reqwest::blocking::Client,
    credentials: credentials::Credentials,
    region: String,
    deadline: Instant,
}
impl Client {
    fn new(credentials: credentials::Credentials, region: String) -> Result<Self> {
        // Only known AWS region-shaped DNS labels; inputs cannot change host,
        // port, path or HTTPS scheme. GovCloud shares the commercial suffix.
        if region.len() > 40
            || !region.contains('-')
            || !region
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(Error("invalid AWS region"));
        }
        let http = reqwest::blocking::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| Error("AWS HTTPS client unavailable"))?;
        Ok(Self {
            http,
            credentials,
            region,
            deadline: Instant::now() + Duration::from_secs(120),
        })
    }
    fn request(&self, service: &str, bucket: Option<&str>, body: &str) -> Result<String> {
        let suffix = if self.region.starts_with("cn-") {
            "amazonaws.com.cn"
        } else {
            "amazonaws.com"
        };
        let host = match bucket {
            Some(bucket) => {
                // One DNS label also avoids wildcard TLS problems for dotted buckets.
                if !(3..=63).contains(&bucket.len())
                    || !bucket
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                    || bucket.starts_with('-')
                    || bucket.ends_with('-')
                {
                    return Err(Error("unsupported S3 bucket name"));
                }
                format!("{bucket}.s3.{}.{suffix}", self.region)
            }
            None => format!("{service}.{}.{suffix}", self.region),
        };
        let method = if service == "s3" { "HEAD" } else { "POST" };
        let mut initial = BTreeMap::new();
        if service == "ec2" {
            initial.insert(
                "content-type".into(),
                "application/x-www-form-urlencoded; charset=utf-8".into(),
            );
        }
        let headers = sign::headers(
            &sign::Request {
                method,
                host: &host,
                service,
                region: &self.region,
                body: body.as_bytes(),
                checksum_header: service == "s3",
            },
            &self.credentials,
            &chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string(),
            initial,
        );
        let mut request = self.http.request(
            if service == "s3" {
                reqwest::Method::HEAD
            } else {
                reqwest::Method::POST
            },
            format!("https://{host}/"),
        );
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or(Error("AWS operation timed out"))?;
        let response = request
            .timeout(remaining.min(Duration::from_secs(30)))
            .body(body.to_owned())
            .send()
            .map_err(|_| Error("AWS HTTPS request failed"))?;
        if !response.status().is_success() {
            return Err(Error("AWS API returned an error"));
        }
        let mut bytes = Vec::new();
        response
            .take(8 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error("AWS response read failed"))?;
        if bytes.len() > 8 * 1024 * 1024 {
            return Err(Error("AWS response too large"));
        }
        String::from_utf8(bytes).map_err(|_| Error("invalid AWS response encoding"))
    }
}

// Injectable API boundary for fully offline response and pagination tests.
fn execute(
    command: &Command,
    mut request: impl FnMut(&str, Option<&str>, &str) -> Result<String>,
) -> Result<String> {
    match &command.operation {
        Operation::Head(bucket) => {
            request("s3", Some(bucket), "")?;
            Ok(String::new())
        }
        Operation::Describe(params) => {
            let mut params = params.clone();
            let mut reservations = Vec::new();
            let mut tokens = BTreeSet::new();
            loop {
                let body = request("ec2", None, &form("DescribeInstances", &params))?;
                let page: response::Describe = response::parse(&body, "DescribeInstancesResponse")?;
                for reservation in page.reservation_set.item {
                    let instances = reservation
                        .instances_set
                        .item
                        .into_iter()
                        .map(response::instance_json)
                        .collect::<Result<Vec<_>>>()?;
                    reservations.push(json!({"Instances": instances}));
                }
                match page.next_token.filter(|t| !t.is_empty()) {
                    None => break,
                    Some(token) => {
                        if tokens.len() >= 1000 || !tokens.insert(token.clone()) {
                            return Err(Error("AWS pagination did not complete"));
                        }
                        params.retain(|(key, _)| key != "NextToken");
                        params.push(("NextToken".into(), token));
                    }
                }
            }
            if command.text {
                Ok(reservations
                    .iter()
                    .flat_map(|r| r["Instances"].as_array().into_iter().flatten())
                    .filter_map(|i| i["InstanceId"].as_str())
                    .collect::<Vec<_>>()
                    .join("\t"))
            } else {
                Ok(json!({"Reservations": reservations}).to_string())
            }
        }
        Operation::Console(params) => {
            let body = request("ec2", None, &form("GetConsoleOutput", params))?;
            let console: response::Console = response::parse(&body, "GetConsoleOutputResponse")?;
            let output = console
                .output
                .map(|encoded| {
                    let encoded: String = encoded
                        .chars()
                        .filter(|c| !c.is_ascii_whitespace())
                        .collect();
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .map_err(|_| Error("invalid AWS console encoding"))?;
                    String::from_utf8(bytes).map_err(|_| Error("invalid AWS console text"))
                })
                .transpose()?;
            if command.text {
                Ok(output.unwrap_or_else(|| "None".into()))
            } else {
                Ok(json!({"Output": output}).to_string())
            }
        }
        Operation::Terminate(params) => {
            let body = request("ec2", None, &form("TerminateInstances", params))?;
            let response: response::Terminate =
                response::parse(&body, "TerminateInstancesResponse")?;
            let instances: Vec<Value> = response.instances_set.item.into_iter().map(|i| json!({"InstanceId": i.instance_id, "CurrentState": {"Name": i.current_state.name}, "PreviousState": {"Name": i.previous_state.name}})).collect();
            Ok(json!({"TerminatingInstances": instances}).to_string())
        }
    }
}

/// AWS errors always produce a nonzero exit and no stdout. In particular a
/// failed describe can never look like the successful empty leftover list.
pub fn run(argv: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32 {
    let result = (|| {
        let command = command(argv)?;
        let (credentials, region) = credentials::load(
            command.profile.as_deref(),
            command.region.as_deref(),
            &|key| std::env::var(key).ok(),
        )?;
        let client = Client::new(credentials, region)?;
        execute(&command, |service, bucket, body| {
            client.request(service, bucket, body)
        })
    })();
    match result {
        Ok(output) => {
            if writeln!(stdout, "{output}").is_ok() {
                0
            } else {
                1
            }
        }
        Err(Error(message)) => {
            let _ = writeln!(stderr, "rsi-cloud: {message}");
            1
        }
    }
}

#[cfg(test)]
mod tests;
