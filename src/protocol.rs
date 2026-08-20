//! The line-JSON protocol spoken over the control socket. Pure data.

use serde::{Deserialize, Serialize};

/// Version number every request carries as `"v"`.
pub const PROTOCOL_VERSION: u32 = 1;

/// A request as it appears on the wire: the version plus the flattened command.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Envelope {
    pub v: u32,
    #[serde(flatten)]
    pub request: Request,
}

/// What a client asks the running instance to do. Tagged by `"cmd"`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    /// Describe every slot.
    Status,
    /// Restart a slot (or `all`) with its current command.
    Restart {
        slot: String,
        #[serde(default)]
        wait: bool,
    },
    /// Kill a slot (or `all`) and return it to its standard command.
    Kill {
        slot: String,
        #[serde(default)]
        wait: bool,
    },
    /// Tear a slot (or `all`) down and leave it dead.
    Stop {
        slot: String,
        #[serde(default)]
        wait: bool,
    },
    /// Revive a dead slot (or `all`).
    Start {
        slot: String,
        #[serde(default)]
        wait: bool,
    },
    /// Run a one-shot override in a slot; standard resumes when it exits.
    /// The argv travels as `"command"` because `"cmd"` is the tag.
    Run {
        slot: String,
        #[serde(default, rename = "command")]
        cmd: Vec<String>,
        #[serde(default)]
        wrap: Option<String>,
        #[serde(default)]
        wait: bool,
    },
    /// Shut the whole instance down, as `q` does.
    Quit,
    /// Fetch the tail of the buffered output.
    Logs {
        #[serde(default)]
        slot: Option<String>,
        #[serde(default = "default_tail")]
        tail: usize,
        #[serde(default)]
        since_ms: Option<u64>,
        #[serde(default)]
        color: bool,
    },
}

fn default_tail() -> usize {
    100
}

/// What the instance answers. Untagged: the variant order matters, each is
/// told apart by a distinguishing field (`error`, `procs`, `lines`, `started`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(untagged)]
pub enum Response {
    /// The request was refused or malformed.
    Error { ok: bool, error: String },
    /// Answer to `status`.
    Status {
        ok: bool,
        pid: u32,
        dir: String,
        procs: Vec<ProcStatus>,
    },
    /// Answer to `logs`.
    Logs { ok: bool, lines: Vec<LogLine> },
    /// Answer to a slot verb: what was started, what was skipped and why.
    Acted {
        ok: bool,
        started: Vec<Started>,
        skipped: Vec<Skipped>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        markers: Option<Vec<String>>,
    },
    /// Plain acknowledgement (`quit`).
    Done { ok: bool },
}

/// One slot as reported by `status`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ProcStatus {
    pub index: usize,
    pub name: String,
    pub health: String,
    pub r#gen: u32,
    pub pid: Option<i32>,
    pub command: String,
    pub standard: String,
    pub r#override: bool,
    pub since_ms: Option<u64>,
}

/// A slot a verb acted on.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Started {
    pub proc: usize,
    pub name: String,
    pub from_gen: Option<u32>,
}

/// A slot a verb left alone, and why.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Skipped {
    pub proc: usize,
    pub name: String,
    pub reason: String,
}

/// One buffered output line as returned by `logs`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct LogLine {
    pub seq: u64,
    pub at_ms: u64,
    pub r#gen: u32,
    pub proc: usize,
    pub name: String,
    pub stream: String,
    pub text: String,
}

impl Response {
    pub fn error(message: impl Into<String>) -> Response {
        Response::Error {
            ok: false,
            error: message.into(),
        }
    }

    pub fn done() -> Response {
        Response::Done { ok: true }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_with_defaults() {
        let e: Envelope =
            serde_json::from_str(r#"{"v":1,"cmd":"restart","slot":"server"}"#).unwrap();
        assert_eq!(e.v, 1);
        assert_eq!(
            e.request,
            Request::Restart {
                slot: "server".into(),
                wait: false
            }
        );

        let e: Envelope = serde_json::from_str(r#"{"v":1,"cmd":"logs"}"#).unwrap();
        assert_eq!(
            e.request,
            Request::Logs {
                slot: None,
                tail: 100,
                since_ms: None,
                color: false
            }
        );

        let e: Envelope = serde_json::from_str(
            r#"{"v":1,"cmd":"run","slot":"server","wrap":"perf record -g","wait":true}"#,
        )
        .unwrap();
        assert_eq!(
            e.request,
            Request::Run {
                slot: "server".into(),
                cmd: vec![],
                wrap: Some("perf record -g".into()),
                wait: true
            }
        );

        let text = serde_json::to_string(&Envelope {
            v: 1,
            request: Request::Status,
        })
        .unwrap();
        assert_eq!(text, r#"{"v":1,"cmd":"status"}"#);

        assert!(serde_json::from_str::<Envelope>(r#"{"v":1,"cmd":"dance"}"#).is_err());
    }

    #[test]
    fn responses_round_trip_and_untagged_order_is_unambiguous() {
        let cases = vec![
            Response::error("nope"),
            Response::Status {
                ok: true,
                pid: 7,
                dir: "/p".into(),
                procs: vec![ProcStatus {
                    index: 1,
                    name: "a".into(),
                    health: "running".into(),
                    r#gen: 2,
                    pid: Some(3),
                    command: "x".into(),
                    standard: "x".into(),
                    r#override: false,
                    since_ms: Some(10),
                }],
            },
            Response::Logs {
                ok: true,
                lines: vec![LogLine {
                    seq: 1,
                    at_ms: 2,
                    r#gen: 0,
                    proc: 0,
                    name: "a".into(),
                    stream: "stdout".into(),
                    text: "hi".into(),
                }],
            },
            Response::Acted {
                ok: true,
                started: vec![Started {
                    proc: 0,
                    name: "a".into(),
                    from_gen: Some(1),
                }],
                skipped: vec![],
                markers: None,
            },
            Response::Acted {
                ok: true,
                started: vec![],
                skipped: vec![Skipped {
                    proc: 1,
                    name: "b".into(),
                    reason: "restart in flight".into(),
                }],
                markers: Some(vec!["── x ──".into()]),
            },
            Response::done(),
        ];
        for r in cases {
            let text = serde_json::to_string(&r).unwrap();
            let back: Response = serde_json::from_str(&text).unwrap();
            assert_eq!(back, r, "{text}");
        }
        let text = serde_json::to_string(&Response::error("nope")).unwrap();
        assert_eq!(text, r#"{"ok":false,"error":"nope"}"#);
        let text = serde_json::to_string(&Response::Status {
            ok: true,
            pid: 1,
            dir: "d".into(),
            procs: vec![],
        })
        .unwrap();
        assert!(
            text.contains(r#""override""#) || !text.contains("r#"),
            "{text}"
        );
    }
}
