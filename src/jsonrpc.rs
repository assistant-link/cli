use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const NOT_RECEIVED: i64 = -32001;

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Request(Request),
    Notification(Notification),
    Response(Response),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub id: Value,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub id: Value,
    pub result: Result<Value, ErrorObject>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Ready(Ready),
    Closing(Closing),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Ready {
    pub link_id: String,
    pub assistant_name: String,
    pub protocol_version: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Closing {
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidMessage(pub String);

#[derive(Deserialize)]
struct Raw {
    jsonrpc: String,
    #[serde(default, deserialize_with = "present")]
    id: Option<Value>,
    method: Option<String>,
    params: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    result: Option<Value>,
    error: Option<ErrorObject>,
}

// Without this, serde reads `"id": null` as an absent id, which would turn a response with a
// null id into an invalid message and a request with a null id into a notification.
fn present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

pub fn parse(text: &str) -> Result<Message, InvalidMessage> {
    let invalid = |reason: &str| InvalidMessage(reason.to_owned());
    let value: Value = serde_json::from_str(text).map_err(|error| invalid(&error.to_string()))?;
    if !value.is_object() {
        return Err(invalid("expected one JSON-RPC message object"));
    }
    let raw = Raw::deserialize(value).map_err(|error| invalid(&error.to_string()))?;
    if raw.jsonrpc != "2.0" {
        return Err(invalid("expected jsonrpc \"2.0\""));
    }
    match (raw.method, raw.id, raw.result, raw.error) {
        (Some(method), Some(id), None, None) => Ok(Message::Request(Request {
            id,
            method,
            params: raw.params,
        })),
        (Some(method), None, None, None) => Ok(Message::Notification(Notification {
            method,
            params: raw.params,
        })),
        (None, Some(id), Some(result), None) => Ok(Message::Response(Response {
            id,
            result: Ok(result),
        })),
        (None, Some(id), None, Some(error)) => Ok(Message::Response(Response {
            id,
            result: Err(error),
        })),
        _ => Err(invalid("not a request, notification, or response")),
    }
}

impl Notification {
    pub fn event(&self) -> Option<Event> {
        let params = self.params.clone()?;
        match self.method.as_str() {
            "connection.ready" => serde_json::from_value(params).ok().map(Event::Ready),
            "connection.closing" => serde_json::from_value(params).ok().map(Event::Closing),
            _ => None,
        }
    }
}

impl Response {
    pub fn method_not_found(id: Value, method: &str) -> Self {
        Self {
            id,
            result: Err(ErrorObject {
                code: METHOD_NOT_FOUND,
                message: format!("Method not found: {method}"),
                data: None,
            }),
        }
    }

    pub fn event_acknowledged(id: Value, event_id: &str) -> Self {
        Self {
            id,
            result: Ok(json!({"event_id": event_id})),
        }
    }

    pub fn not_received(id: Value) -> Self {
        Self {
            id,
            result: Err(ErrorObject {
                code: NOT_RECEIVED,
                message: "The event was not received.".to_owned(),
                data: Some(json!({"reason": "not_received"})),
            }),
        }
    }

    pub fn invalid_params(id: Value) -> Self {
        Self {
            id,
            result: Err(ErrorObject {
                code: INVALID_PARAMS,
                message: "Invalid params: expected an event with an id.".to_owned(),
                data: None,
            }),
        }
    }

    pub fn encode(&self) -> String {
        match &self.result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": self.id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": self.id, "error": error}),
        }
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(text: &str) -> Notification {
        match parse(text).unwrap() {
            Message::Notification(notification) => notification,
            other => panic!("expected a notification, got {other:?}"),
        }
    }

    #[test]
    fn parses_connection_ready() {
        let message = notification(
            r#"{"jsonrpc":"2.0","method":"connection.ready",
                "params":{"link_id":"l1","assistant_name":"Hermes","protocol_version":"1"}}"#,
        );
        assert_eq!(
            message.event(),
            Some(Event::Ready(Ready {
                link_id: "l1".into(),
                assistant_name: "Hermes".into(),
                protocol_version: "1".into(),
            }))
        );
    }

    #[test]
    fn parses_connection_closing() {
        let message = notification(
            r#"{"jsonrpc":"2.0","method":"connection.closing","params":{"reason":"account_deleted"}}"#,
        );
        assert_eq!(
            message.event(),
            Some(Event::Closing(Closing {
                reason: "account_deleted".into()
            }))
        );
    }

    #[test]
    fn ignores_unknown_fields() {
        let message = notification(
            r#"{"jsonrpc":"2.0","method":"connection.closing","extra":true,
                "params":{"reason":"maintenance","retry_after":30}}"#,
        );
        assert_eq!(
            message.event(),
            Some(Event::Closing(Closing {
                reason: "maintenance".into()
            }))
        );
    }

    #[test]
    fn unknown_notifications_parse_without_an_event() {
        let message =
            notification(r#"{"jsonrpc":"2.0","method":"message.new","params":{"id":"m1"}}"#);
        assert_eq!(message.method, "message.new");
        assert_eq!(message.event(), None);
    }

    #[test]
    fn known_notification_with_invalid_params_has_no_event() {
        let message = notification(r#"{"jsonrpc":"2.0","method":"connection.ready","params":{}}"#);
        assert_eq!(message.event(), None);
    }

    #[test]
    fn parses_error_responses() {
        let message = parse(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32001,"message":"Nope.",
                "data":{"reason":"not_allowed"}}}"#,
        )
        .unwrap();
        assert_eq!(
            message,
            Message::Response(Response {
                id: json!(1),
                result: Err(ErrorObject {
                    code: -32001,
                    message: "Nope.".into(),
                    data: Some(json!({"reason": "not_allowed"})),
                }),
            })
        );
    }

    #[test]
    fn parses_error_responses_with_a_null_id() {
        let message = parse(
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error."}}"#,
        )
        .unwrap();
        assert!(matches!(
            message,
            Message::Response(Response {
                id: Value::Null,
                result: Err(_)
            })
        ));
    }

    #[test]
    fn parses_success_responses() {
        let message = parse(r#"{"jsonrpc":"2.0","id":"a","result":{"ok":true}}"#).unwrap();
        assert_eq!(
            message,
            Message::Response(Response {
                id: json!("a"),
                result: Ok(json!({"ok": true})),
            })
        );
    }

    #[test]
    fn parses_requests_from_the_service() {
        let message =
            parse(r#"{"jsonrpc":"2.0","id":7,"method":"device.ping","params":[1]}"#).unwrap();
        assert_eq!(
            message,
            Message::Request(Request {
                id: json!(7),
                method: "device.ping".into(),
                params: Some(json!([1])),
            })
        );
    }

    #[test]
    fn rejects_invalid_messages() {
        for text in [
            "not json",
            "[]",
            r#"[{"jsonrpc":"2.0","method":"connection.ready"}]"#,
            r#"{"method":"connection.ready"}"#,
            r#"{"jsonrpc":"1.0","method":"connection.ready"}"#,
            r#"{"jsonrpc":"2.0"}"#,
            r#"{"jsonrpc":"2.0","method":5}"#,
            r#"{"jsonrpc":"2.0","id":1}"#,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":"x"}}"#,
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn encodes_the_event_acknowledgement_to_any_kind_of_id() {
        for id in [json!(7), json!("a-1"), Value::Null] {
            let response = Response::event_acknowledged(id.clone(), "0199");
            let encoded: Value = serde_json::from_str(&response.encode()).unwrap();
            assert_eq!(
                encoded,
                json!({"jsonrpc": "2.0", "id": id, "result": {"event_id": "0199"}})
            );
        }
    }

    #[test]
    fn encodes_not_received() {
        let encoded: Value =
            serde_json::from_str(&Response::not_received(json!(8)).encode()).unwrap();
        assert_eq!(encoded["id"], 8);
        assert_eq!(encoded["error"]["code"], -32001);
        assert_eq!(encoded["error"]["data"], json!({"reason": "not_received"}));
    }

    #[test]
    fn encodes_invalid_params() {
        let encoded: Value =
            serde_json::from_str(&Response::invalid_params(json!(8)).encode()).unwrap();
        assert_eq!(encoded["error"]["code"], -32602);
    }

    #[test]
    fn encodes_method_not_found() {
        let response = Response::method_not_found(json!(7), "device.ping");
        let encoded: Value = serde_json::from_str(&response.encode()).unwrap();
        assert_eq!(
            encoded,
            json!({
                "jsonrpc": "2.0",
                "id": 7,
                "error": {"code": -32601, "message": "Method not found: device.ping"}
            })
        );
    }
}
