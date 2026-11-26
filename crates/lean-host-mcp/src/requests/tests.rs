use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Map, Value, json};

use super::{
    RequestError, RequestSettings, StoreError, canonical_json, check_id, error_result, id_from, intent, server_id,
    stamp_error, stamp_result,
};
use crate::config_file::RequestsFileConfig;
use crate::server::carry;
use crate::tools::ResponseCarrier;

const ID: &str = "req-00001";

fn object(value: Value) -> anyhow::Result<Map<String, Value>> {
    if let Value::Object(map) = value {
        Ok(map)
    } else {
        anyhow::bail!("not an object: {value}")
    }
}

/// The JSON object in the first text item of `result`.
fn first_text(result: &CallToolResult) -> anyhow::Result<Value> {
    let text = result
        .content
        .first()
        .and_then(ContentBlock::as_text)
        .ok_or_else(|| anyhow::anyhow!("no text item"))?;
    Ok(serde_json::from_str(&text.text)?)
}

#[test]
fn ids_follow_the_rule_and_srv_ids_belong_to_the_server() -> anyhow::Result<()> {
    let longest = "x".repeat(128);
    for good in ["abcdefgh", "a.b_c:d-e", longest.as_str()] {
        check_id(good).map_err(|error| anyhow::anyhow!("{good}: {}", error.message))?;
    }
    let too_long = "x".repeat(129);
    for bad in [
        "short",
        too_long.as_str(),
        "has space1",
        "caf\u{e9}-1234",
        "srv-12345678",
    ] {
        assert_eq!(
            check_id(bad).map_err(|error| error.code),
            Err("invalid_request_id"),
            "{bad}"
        );
    }
    assert!(server_id().starts_with("srv-"));
    assert_eq!(
        id_from(&json!(12_345_678)).map_err(|error| error.code),
        Err("invalid_request_id")
    );
    assert_eq!(id_from(&json!(ID)).map_err(|error| error.code), Ok(ID.to_owned()));
    Ok(())
}

#[test]
fn intent_sorts_keys_at_every_level() -> anyhow::Result<()> {
    let one = object(json!({"b": "\u{e9}\n", "a": {"y": [2, {"q": null, "p": true}], "x": 1}}))?;
    let other = object(json!({"a": {"x": 1, "y": [2, {"p": true, "q": null}]}, "b": "\u{e9}\n"}))?;
    let mut text = String::new();
    canonical_json(&json!({"tool": "lean_build", "arguments": one}), &mut text);
    assert_eq!(
        text,
        "{\"arguments\":{\"a\":{\"x\":1,\"y\":[2,{\"p\":true,\"q\":null}]},\"b\":\"\u{e9}\\n\"},\"tool\":\"lean_build\"}"
    );
    // SHA-256 of that text, from `shasum -a 256`.
    let digest = "e1bfa52dccc8796ce072600de2f452cf8fb7e42ac01e53f6d5c2b52768b2aacd";
    assert_eq!(intent("lean_build", &one), digest);
    assert_eq!(intent("lean_build", &other), digest);
    assert_ne!(intent("lean_verify", &one), digest);
    // Array order is part of the call.
    let swapped = object(json!({"b": "\u{e9}\n", "a": {"y": [{"q": null, "p": true}, 2], "x": 1}}))?;
    assert_ne!(intent("lean_build", &swapped), digest);
    Ok(())
}

#[test]
fn every_reply_carries_its_id_at_the_top() -> anyhow::Result<()> {
    for carrier in [
        ResponseCarrier::Text,
        ResponseCarrier::Both,
        ResponseCarrier::Structured,
    ] {
        let mut result = carry(&json!({"ok": true}), carrier);
        stamp_result(&mut result, ID);
        if let Some(structured) = &result.structured_content {
            assert_eq!(structured.get("request_id"), Some(&json!(ID)), "{carrier:?}");
        }
        if !result.content.is_empty() {
            assert_eq!(first_text(&result)?.get("request_id"), Some(&json!(ID)), "{carrier:?}");
        }
    }

    // rmcp reports undecodable arguments as plain text.
    let mut result = CallToolResult::error(vec![ContentBlock::text(
        "failed to deserialize parameters: missing field `files`",
    )]);
    stamp_result(&mut result, ID);
    let body = first_text(&result)?;
    assert_eq!(body.pointer("/error/code"), Some(&json!("invalid_arguments")));
    assert_eq!(body.get("request_id"), Some(&json!(ID)));

    // Plain text that is not an error stays as it is.
    let mut result = CallToolResult::success(vec![ContentBlock::text("plain")]);
    stamp_result(&mut result, ID);
    assert_eq!(
        result
            .content
            .first()
            .and_then(ContentBlock::as_text)
            .map(|text| text.text.as_str()),
        Some("plain")
    );

    let mut error = McpError::invalid_params("tool not found", None);
    stamp_error(&mut error, ID);
    assert_eq!(error.data, Some(json!({"request_id": ID})));
    let mut error = McpError::internal_error("busy", Some(json!({"reason": "r"})));
    stamp_error(&mut error, ID);
    assert_eq!(error.data, Some(json!({"reason": "r", "request_id": ID})));
    let mut error = McpError::internal_error("busy", Some(json!("text")));
    stamp_error(&mut error, ID);
    assert_eq!(error.data, Some(json!({"detail": "text", "request_id": ID})));
    Ok(())
}

#[test]
fn a_refusal_is_a_tool_error_the_client_can_read() -> anyhow::Result<()> {
    let reply = RequestError::store(&StoreError::Task("gone".to_owned())).reply(ID, ResponseCarrier::Text);
    assert_eq!(reply.is_error, Some(true));
    let body = first_text(&reply)?;
    assert_eq!(body.pointer("/error/code"), Some(&json!("request_store_failed")));
    assert_eq!(body.pointer("/error/retryable"), Some(&json!(true)));
    assert_eq!(body.pointer("/error/retry_after_millis"), Some(&json!(1000)));
    assert_eq!(body.get("request_id"), Some(&json!(ID)));

    let reply = RequestError::unavailable().reply(ID, ResponseCarrier::Structured);
    let body = reply
        .structured_content
        .ok_or_else(|| anyhow::anyhow!("no structured content"))?;
    assert_eq!(body.pointer("/error/code"), Some(&json!("requests_unavailable")));
    assert_eq!(body.pointer("/error/retryable"), None);
    Ok(())
}

#[test]
fn protocol_errors_keep_their_data_and_gain_a_code() -> anyhow::Result<()> {
    let result = error_result(&McpError::invalid_params("bad", None), ResponseCarrier::Text);
    assert_eq!(result.is_error, Some(true));
    let body = first_text(&result)?;
    assert_eq!(body.pointer("/error/code"), Some(&json!("invalid_arguments")));
    assert_eq!(body.pointer("/error/message"), Some(&json!("bad")));

    let busy = McpError::internal_error(
        "no turn",
        Some(json!({"reason": "project_admission_timeout", "retryable": true, "blocked_by": null})),
    );
    let body = first_text(&error_result(&busy, ResponseCarrier::Text))?;
    assert_eq!(body.pointer("/error/code"), Some(&json!("project_admission_timeout")));
    assert_eq!(body.pointer("/error/retryable"), Some(&json!(true)));
    assert_eq!(body.pointer("/error/blocked_by"), None);
    Ok(())
}

#[test]
fn settings_keep_defaults_and_refuse_zero() {
    assert_eq!(
        RequestSettings::from_file(&RequestsFileConfig::default()),
        Ok(RequestSettings::default())
    );
    let set = |retention_hours, max_rows, wait_max_secs| {
        RequestSettings::from_file(&RequestsFileConfig {
            retention_hours,
            max_rows,
            wait_max_secs,
        })
    };
    assert!(set(Some(0), None, None).is_err());
    assert!(set(None, Some(0), None).is_err());
    assert!(set(None, None, Some(0)).is_err());
    let custom = set(Some(1), Some(5), Some(7));
    assert_eq!(custom.map(|settings| settings.max_rows), Ok(5));
}
