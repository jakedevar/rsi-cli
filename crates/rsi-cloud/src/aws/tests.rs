use super::*;

fn cmd(args: &[&str]) -> Command {
    command(&args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>()).unwrap()
}
fn leftovers() -> Command {
    cmd(&[
        "ec2",
        "describe-instances",
        "--filters",
        "Name=tag:Service,Values=rsi-pool",
        "Name=instance-state-name,Values=pending,running,stopping,stopped",
        "--query",
        "Reservations[].Instances[].InstanceId",
        "--output",
        "text",
    ])
}
const DESCRIBE: &str = include_str!("../../tests/fixtures/aws/describe.xml");
const EMPTY: &str = "<DescribeInstancesResponse><reservationSet/></DescribeInstancesResponse>";

#[test]
fn aws_published_sigv4_vectors() {
    // Unmodified AWS SDK test-suite vectors; these credentials are published examples.
    for (context, request, signature) in [
        (
            include_str!("../../tests/fixtures/aws/get-vanilla/context.json"),
            include_str!("../../tests/fixtures/aws/get-vanilla/request.txt"),
            include_str!("../../tests/fixtures/aws/get-vanilla/header-signature.txt"),
        ),
        (
            include_str!("../../tests/fixtures/aws/post-vanilla/context.json"),
            include_str!("../../tests/fixtures/aws/post-vanilla/request.txt"),
            include_str!("../../tests/fixtures/aws/post-vanilla/header-signature.txt"),
        ),
        (
            include_str!("../../tests/fixtures/aws/post-x-www-form-urlencoded/context.json"),
            include_str!("../../tests/fixtures/aws/post-x-www-form-urlencoded/request.txt"),
            include_str!(
                "../../tests/fixtures/aws/post-x-www-form-urlencoded/header-signature.txt"
            ),
        ),
        (
            include_str!("../../tests/fixtures/aws/get-vanilla-with-session-token/context.json"),
            include_str!("../../tests/fixtures/aws/get-vanilla-with-session-token/request.txt"),
            include_str!(
                "../../tests/fixtures/aws/get-vanilla-with-session-token/header-signature.txt"
            ),
        ),
    ] {
        let context: Value = serde_json::from_str(context).unwrap();
        let (head, body) = request.split_once("\n\n").unwrap_or((request, ""));
        let mut lines = head.lines();
        let method = lines.next().unwrap().split_whitespace().next().unwrap();
        let mut headers = BTreeMap::new();
        for line in lines {
            let (key, value) = line.split_once(':').unwrap();
            headers.insert(key.to_ascii_lowercase(), value.trim().to_owned());
        }
        let host = headers.remove("host").unwrap();
        let creds = credentials::Credentials {
            access: context["credentials"]["access_key_id"]
                .as_str()
                .unwrap()
                .into(),
            secret: context["credentials"]["secret_access_key"]
                .as_str()
                .unwrap()
                .into(),
            token: context["credentials"]["token"].as_str().map(str::to_owned),
        };
        let timestamp =
            chrono::DateTime::parse_from_rfc3339(context["timestamp"].as_str().unwrap())
                .unwrap()
                .format("%Y%m%dT%H%M%SZ")
                .to_string();
        let signed = sign::headers(
            &sign::Request {
                method,
                host: &host,
                service: context["service"].as_str().unwrap(),
                region: context["region"].as_str().unwrap(),
                body: body.as_bytes(),
                checksum_header: context["sign_body"].as_bool().unwrap(),
            },
            &creds,
            &timestamp,
            headers,
        );
        assert_eq!(
            signed["authorization"].split("Signature=").nth(1).unwrap(),
            signature.trim()
        );
    }
}

#[test]
fn describe_fixture_preserves_state_and_spot_reason() {
    let command = cmd(&[
        "ec2",
        "describe-instances",
        "--instance-ids",
        "i-1234567890abcdef0",
        "--output",
        "json",
    ]);
    let output = execute(&command, |service, bucket, body| {
        assert_eq!(service, "ec2");
        assert_eq!(bucket, None);
        assert_eq!(
            body,
            "Action=DescribeInstances&Version=2016-11-15&InstanceId.1=i-1234567890abcdef0"
        );
        Ok(DESCRIBE.into())
    })
    .unwrap();
    let json: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(
        json["Reservations"][0]["Instances"][0]["State"]["Name"],
        "terminated"
    );
    assert_eq!(
        json["Reservations"][0]["Instances"][0]["StateReason"]["Code"],
        "Server.SpotInstanceTermination"
    );
    assert_eq!(
        execute(&leftovers(), |_, _, _| Ok(DESCRIBE.into())).unwrap(),
        "i-1234567890abcdef0"
    );
    assert_eq!(
        execute(&leftovers(), |_, _, _| Ok(EMPTY.into())).unwrap(),
        ""
    );
}

#[test]
fn pagination_buffers_all_results_and_fails_closed_on_later_error() {
    let first = DESCRIBE.replace(
        "</DescribeInstancesResponse>",
        "<nextToken>a+/=&amp;</nextToken></DescribeInstancesResponse>",
    );
    let mut count = 0;
    let output = execute(&leftovers(), |_, _, body| {
        count += 1;
        if count == 1 {
            assert!(body.contains("Filter.1.Name=tag%3AService"));
            assert!(body.contains("Filter.2.Value.4=stopped"));
            assert!(body.contains("MaxResults=1000"));
            Ok(first.clone())
        } else {
            assert!(body.contains("NextToken=a%2B%2F%3D%26"));
            Ok(EMPTY.into())
        }
    })
    .unwrap();
    assert_eq!(count, 2);
    assert_eq!(output, "i-1234567890abcdef0");
    let mut count = 0;
    assert!(
        execute(&leftovers(), |_, _, _| {
            count += 1;
            if count == 1 {
                Ok(first.clone())
            } else {
                Err(Error("API failure"))
            }
        })
        .is_err()
    );
    assert!(execute(&leftovers(), |_, _, _| Ok(first.clone())).is_err());
}

#[test]
fn invalid_success_responses_never_mean_empty() {
    for body in [
        "",
        "<html/>",
        "<Response><Errors/></Response>",
        "<DescribeInstancesResponse/>",
        "<DescribeInstancesResponse><reservationSet>",
        "<DescribeInstancesResponse><reservationSet><item/></reservationSet></DescribeInstancesResponse>",
        "<DescribeInstancesResponse><reservationSet><item><instancesSet><item><instanceId/></item></instancesSet></item></reservationSet></DescribeInstancesResponse>",
    ] {
        assert!(
            execute(&leftovers(), |_, _, _| Ok(body.into())).is_err(),
            "{body}"
        );
    }
    assert!(
        execute(&leftovers(), |_, _, _| Err(Error(
            "credential/API/transport failure"
        )))
        .is_err()
    );
    assert!(execute(&leftovers(), |_, _, _| Ok(format!("{EMPTY}{EMPTY}"))).is_err());
}

#[test]
fn console_is_decoded_and_termination_and_head_match_script_shapes() {
    let command = cmd(&[
        "ec2",
        "get-console-output",
        "--instance-id",
        "i-1234567890abcdef0",
        "--latest",
        "--query",
        "Output",
        "--output",
        "text",
    ]);
    let console = execute(&command, |_, _, body| {
        assert!(body.contains("Latest=true"));
        Ok(include_str!("../../tests/fixtures/aws/console.xml").into())
    })
    .unwrap();
    assert!(console.starts_with("Linux version 2.6.16"));
    let terminate = cmd(&[
        "ec2",
        "terminate-instances",
        "--instance-ids",
        "i-1234567890abcdef0",
    ]);
    let output = execute(&terminate, |_, _, body| {
        assert!(body.starts_with("Action=TerminateInstances&"));
        Ok(include_str!("../../tests/fixtures/aws/terminate.xml").into())
    })
    .unwrap();
    let value: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(
        value["TerminatingInstances"][0]["CurrentState"]["Name"],
        "shutting-down"
    );
    let head = cmd(&["s3api", "head-bucket", "--bucket", "rsi-cache"]);
    assert_eq!(
        execute(&head, |service, bucket, body| {
            assert_eq!((service, bucket, body), ("s3", Some("rsi-cache"), ""));
            Ok(String::new())
        })
        .unwrap(),
        ""
    );
    assert!(execute(&head, |_, _, _| Err(Error("denied"))).is_err());
}

#[test]
fn form_encoding_and_s3_signing() {
    assert_eq!(encode("a +/:,=&"), "a%20%2B%2F%3A%2C%3D%26");
    let creds = credentials::Credentials {
        access: "example".into(),
        secret: "example".into(),
        token: Some("temporary".into()),
    };
    let signed = sign::headers(
        &sign::Request {
            method: "HEAD",
            host: "cache.s3.us-west-1.amazonaws.com",
            service: "s3",
            region: "us-west-1",
            body: &[],
            checksum_header: true,
        },
        &creds,
        "20150830T123600Z",
        BTreeMap::new(),
    );
    assert_eq!(
        signed["x-amz-content-sha256"],
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(signed["x-amz-security-token"], "temporary");
    assert!(
        signed["authorization"]
            .contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token")
    );
}

#[test]
fn environment_credentials_need_no_home_and_invalid_characters_fail() {
    let env = |key: &str| match key {
        "AWS_ACCESS_KEY_ID" => Some("test-access".into()),
        "AWS_SECRET_ACCESS_KEY" => Some("test-secret".into()),
        _ => None,
    };
    let (creds, region) = credentials::load(None, Some("us-west-1"), &env).unwrap();
    assert_eq!(creds.access, "test-access");
    assert_eq!(region, "us-west-1");
    assert!(
        credentials::load(
            None,
            Some("us-west-1"),
            &|key| if key == "AWS_SESSION_TOKEN" {
                Some("invalid\ntoken".into())
            } else {
                env(key)
            }
        )
        .is_err()
    );
}

#[test]
fn unsupported_options_and_outputs_are_rejected_before_credentials_or_network() {
    for args in [
        vec!["ec2", "describe-instances"],
        vec![
            "ec2",
            "describe-instances",
            "--instance-ids",
            "i-example",
            "--latest",
        ],
        vec![
            "ec2",
            "terminate-instances",
            "--instance-ids",
            "i-example",
            "--filters",
            "Name=tag:Service,Values=rsi-pool",
        ],
        vec![
            "ec2",
            "describe-instances",
            "--instance-ids",
            "i-example",
            "--query",
            "anything",
            "--output",
            "text",
        ],
    ] {
        assert!(command(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_err());
    }
}
