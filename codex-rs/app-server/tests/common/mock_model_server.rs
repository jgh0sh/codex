use serde_json::json;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const MEMORIES_PROMPT_MARKER: &str =
    "You are extracting durable memories from the user's latest messages";
const MEMORIES_FALLBACK_RESPONSE: &str = "NO_MEMORIES";

/// Create a mock server that will provide the responses, in order, for
/// requests to the `/v1/chat/completions` endpoint.
pub async fn create_mock_chat_completions_server(responses: Vec<String>) -> MockServer {
    let server = MockServer::start().await;

    let num_calls = responses.len();
    let seq_responder = SeqResponder {
        num_calls: AtomicUsize::new(0),
        responses,
    };

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(seq_responder)
        .expect(num_calls as u64..)
        .mount(&server)
        .await;

    server
}

/// Same as `create_mock_chat_completions_server` but does not enforce an
/// expectation on the number of calls.
pub async fn create_mock_chat_completions_server_unchecked(responses: Vec<String>) -> MockServer {
    let server = MockServer::start().await;

    let seq_responder = SeqResponder {
        num_calls: AtomicUsize::new(0),
        responses,
    };

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(seq_responder)
        .mount(&server)
        .await;

    server
}

struct SeqResponder {
    num_calls: AtomicUsize,
    responses: Vec<String>,
}

impl Respond for SeqResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        if is_memories_request(request) {
            return build_final_message_response(MEMORIES_FALLBACK_RESPONSE);
        }

        let call_num = self.num_calls.fetch_add(1, Ordering::SeqCst);
        match self.responses.get(call_num) {
            Some(response) => ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(response.clone(), "text/event-stream"),
            None => panic!("no response for {call_num}"),
        }
    }
}

fn is_memories_request(request: &wiremock::Request) -> bool {
    let body = String::from_utf8_lossy(&request.body);
    body.contains(MEMORIES_PROMPT_MARKER)
}

fn build_final_message_response(message: &str) -> ResponseTemplate {
    let assistant_message = json!({
        "choices": [
            {
                "delta": {
                    "content": message
                },
                "finish_reason": "stop"
            }
        ]
    });

    let sse = format!(
        "data: {}\n\ndata: DONE\n\n",
        serde_json::to_string(&assistant_message).unwrap_or_default()
    );

    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(sse, "text/event-stream")
}
