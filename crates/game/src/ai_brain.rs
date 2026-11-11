//! The AI seat's optional language-model layer. The model only ever *advises*:
//! it answers with a few structured tool calls, each checked here against what
//! the game currently allows (a lead target must be a tag that exists in the
//! area, a reply number must be one the conversation offers) before the seat
//! acts on it. An invalid call is dropped and counted. With no model
//! configured, a slow one or a bad answer the seat plays on with its own
//! rules (`ai_seat`), so the partner never depends on the network.
//!
//! The call goes to an OpenAI-compatible chat endpoint named by
//! `KJK_LLM_URL` (default none: the layer is off), the model by
//! `KJK_LLM_MODEL`, and an optional key by `KJK_LLM_KEY`. It runs on its own
//! thread through `curl`, so the game loop never waits for it.

use std::collections::BTreeSet;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};

use serde_json::{Value, json};

/// One thing the model may ask of the seat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tool {
    /// Follow the human again (ends a hold or a lead).
    Follow,
    /// Stand where the seat is.
    Hold,
    /// Lead the human to the object with this tag.
    LeadTo(String),
    /// Answer the conversation's offer with this reply (zero-based).
    Pick(usize),
    /// Use a medpac now if one is carried.
    Heal,
}

/// What the game allows right now: the checks a call must pass.
#[derive(Debug, Clone, Default)]
pub struct Legal {
    /// Lowercase tags of the area's placed objects.
    pub tags: BTreeSet<String>,
    /// How many replies the open conversation offers (0: none).
    pub replies: usize,
}

/// The calls in `text` that pass `legal`, and how many were dropped.
/// `text` is the model's answer: a JSON object `{"calls": [...]}` (or the
/// array), possibly inside a code fence or after some prose.
pub fn validate(text: &str, legal: &Legal) -> (Vec<Tool>, usize) {
    let Some(value) = first_json(text) else {
        return (Vec::new(), 1);
    };
    let calls = match &value {
        Value::Array(list) => list.clone(),
        Value::Object(map) => match map.get("calls") {
            Some(Value::Array(list)) => list.clone(),
            _ => vec![value.clone()],
        },
        _ => return (Vec::new(), 1),
    };
    let (mut kept, mut dropped) = (Vec::new(), 0);
    for call in calls {
        let name = call
            .get("tool")
            .or_else(|| call.get("name"))
            .and_then(Value::as_str)
            .map(str::to_ascii_lowercase);
        let tool = match name.as_deref() {
            Some("follow") => Some(Tool::Follow),
            Some("hold") => Some(Tool::Hold),
            Some("heal") => Some(Tool::Heal),
            Some("lead_to") => call
                .get("tag")
                .and_then(Value::as_str)
                .map(str::to_ascii_lowercase)
                .filter(|tag| legal.tags.contains(tag))
                .map(Tool::LeadTo),
            Some("pick") => call
                .get("reply")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                // The model counts from one, as the screen does.
                .filter(|n| (1..=legal.replies).contains(n))
                .map(|n| Tool::Pick(n - 1)),
            _ => None,
        };
        match tool {
            Some(tool) => kept.push(tool),
            None => dropped += 1,
        }
    }
    (kept, dropped)
}

/// The first JSON value in `text` (a fence or a sentence may surround it).
fn first_json(text: &str) -> Option<Value> {
    let start = text.find(['{', '['])?;
    let open = text.as_bytes()[start];
    let close = if open == b'{' { b'}' } else { b']' };
    let end = text.rfind(close as char)?;
    (end > start)
        .then(|| serde_json::from_str(&text[start..=end]).ok())
        .flatten()
}

/// The text the model gets as instructions.
pub const SYSTEM_PROMPT: &str = "You are the AI co-op partner of a human player in Star Wars: Knights of the Old Republic. \
You only advise; the game checks every call. Answer with JSON only: {\"calls\": [ ... ]}. \
Tools: {\"tool\":\"follow\"}, {\"tool\":\"hold\"}, {\"tool\":\"heal\"}, \
{\"tool\":\"lead_to\",\"tag\":\"<a tag from the list given>\"}, \
{\"tool\":\"pick\",\"reply\":<number of one offered reply, counting from 1>}. \
Prefer few calls. When a conversation offers replies, you are told whether the human handed the choice to you and which side to favour.";

/// The model's reply to one request, or why there is none.
#[derive(Debug)]
pub struct Answer {
    /// What the caller tagged the request with.
    pub id: u64,
    pub text: Result<String, String>,
}

/// The model on its own thread.
pub struct Llm {
    requests: Sender<(u64, String)>,
    answers: Mutex<Receiver<Answer>>,
    pub model: String,
}

impl Llm {
    /// The layer as the environment names it; none when `KJK_LLM_URL` is
    /// unset.
    pub fn from_env() -> Option<Llm> {
        let url = std::env::var("KJK_LLM_URL").ok().filter(|url| !url.is_empty())?;
        let model = std::env::var("KJK_LLM_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into());
        let key = std::env::var("KJK_LLM_KEY").ok().filter(|key| !key.is_empty());
        let (request_tx, request_rx) = channel::<(u64, String)>();
        let (answer_tx, answer_rx) = channel::<Answer>();
        let name = model.clone();
        std::thread::spawn(move || {
            while let Ok((id, prompt)) = request_rx.recv() {
                let text = ask(&url, &name, key.as_deref(), &prompt);
                if answer_tx.send(Answer { id, text }).is_err() {
                    break;
                }
            }
        });
        Some(Llm {
            requests: request_tx,
            answers: Mutex::new(answer_rx),
            model,
        })
    }

    /// Asks the model; the answer comes later through `poll`.
    pub fn submit(&self, id: u64, prompt: String) {
        let _ = self.requests.send((id, prompt));
    }

    /// An answer that has come, if any.
    pub fn poll(&self) -> Option<Answer> {
        self.answers.lock().ok()?.try_recv().ok()
    }
}

/// One chat-completion call through `curl` (20 s at most).
fn ask(url: &str, model: &str, key: Option<&str>, prompt: &str) -> Result<String, String> {
    let body = json!({
        "model": model,
        "temperature": 0.2,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": prompt},
        ],
    });
    let mut command = Command::new("curl");
    command
        .args(["-sS", "--max-time", "20", "-X", "POST", url])
        .args(["-H", "Content-Type: application/json"]);
    if let Some(key) = key {
        command.args(["-H", &format!("Authorization: Bearer {key}")]);
    }
    let mut child = command
        .args(["--data-binary", "@-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("curl did not start: {error}"))?;
    child
        .stdin
        .take()
        .ok_or("no stdin")?
        .write_all(body.to_string().as_bytes())
        .map_err(|error| error.to_string())?;
    let out = child.wait_with_output().map_err(|error| error.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let reply: Value = serde_json::from_slice(&out.stdout)
        .map_err(|error| format!("the reply is not JSON: {error}"))?;
    reply
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            let detail = reply.pointer("/error/message").and_then(Value::as_str);
            format!("no answer in the reply: {}", detail.unwrap_or("unknown"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legal() -> Legal {
        Legal {
            tags: ["tat17_11guard_01", "k45_door_bast1"]
                .iter()
                .map(|tag| tag.to_string())
                .collect(),
            replies: 3,
        }
    }

    #[test]
    fn valid_calls_pass() {
        let text = r#"{"calls":[{"tool":"lead_to","tag":"K45_DOOR_BAST1"},{"tool":"pick","reply":2},{"tool":"heal"}]}"#;
        let (calls, dropped) = validate(text, &legal());
        assert_eq!(dropped, 0);
        assert_eq!(
            calls,
            vec![Tool::LeadTo("k45_door_bast1".into()), Tool::Pick(1), Tool::Heal]
        );
    }

    #[test]
    fn invented_tag_and_out_of_range_reply_are_dropped() {
        let text = r#"```json
{"calls":[{"tool":"lead_to","tag":"no_such_object"},{"tool":"pick","reply":4},{"tool":"pick","reply":0},{"tool":"teleport"},{"tool":"hold"}]}
```"#;
        let (calls, dropped) = validate(text, &legal());
        assert_eq!(calls, vec![Tool::Hold]);
        assert_eq!(dropped, 4);
    }

    #[test]
    fn prose_without_json_gives_nothing() {
        let (calls, dropped) = validate("I would follow the leader.", &legal());
        assert!(calls.is_empty());
        assert_eq!(dropped, 1);
    }

    #[test]
    fn a_bare_array_and_prose_around_it_work() {
        let (calls, _) = validate(r#"Sure: [{"tool":"follow"}] done"#, &legal());
        assert_eq!(calls, vec![Tool::Follow]);
    }

    #[test]
    fn pick_with_no_conversation_is_dropped() {
        let none = Legal {
            replies: 0,
            ..legal()
        };
        let (calls, dropped) = validate(r#"{"calls":[{"tool":"pick","reply":1}]}"#, &none);
        assert!(calls.is_empty());
        assert_eq!(dropped, 1);
    }

    /// Opt-in (`--ignored`, with `KJK_LLM_URL` set): the real call, through
    /// `curl` to the endpoint, and its answer through the checks.
    #[test]
    #[ignore]
    fn live_model_answers_pass_the_checks() {
        let Some(llm) = Llm::from_env() else {
            panic!("KJK_LLM_URL is not set");
        };
        llm.submit(
            7,
            "The human handed this conversation choice to you; favour the light side. Offered replies:\n1. I will kill him.\n2. Let him go; there has been enough death.\n3. Say nothing.\nAnswer with one pick call.".into(),
        );
        let started = std::time::Instant::now();
        let answer = loop {
            if let Some(answer) = llm.poll() {
                break answer;
            }
            assert!(started.elapsed().as_secs() < 40, "no answer in 40 s");
            std::thread::sleep(std::time::Duration::from_millis(100));
        };
        assert_eq!(answer.id, 7);
        let text = answer.text.expect("the call failed");
        eprintln!("model said: {text}");
        let (calls, dropped) = validate(&text, &Legal { replies: 3, ..Legal::default() });
        eprintln!("kept {calls:?}, dropped {dropped}");
        assert!(!calls.is_empty() && dropped == 0);
    }
}
