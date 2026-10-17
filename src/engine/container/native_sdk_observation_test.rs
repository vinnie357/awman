//! Issue 230: lease-free native ACK observation through the actual stdout callbacks.
//! These unit tests establish ACK evidence, not child retirement or publication.

use super::io_bridge::{PipedReadEnd, PipedStdoutObserver};
use super::native_sdk_execution::NativeSdkObservation;
use crate::engine::agent::agent_matrix::NativePrintRequest;
use crate::engine::error::EngineError;
use std::sync::Arc;

type TestResult = Result<(), EngineError>;
type ProviderCase = (&'static str, &'static [u8], &'static [u8]);
const SESSION: &str = "native-observation-session";

fn cases() -> [ProviderCase; 3] {
    [
        (
            "claude",
            b"{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"native-observation-session\"}\n",
            b"{\"type\":\"result\",\"is_error\":false,\"session_id\":\"native-observation-session\"}\n",
        ),
        (
            "codex",
            b"{\"type\":\"thread.started\",\"thread_id\":\"native-observation-session\"}\n{\"type\":\"turn.started\"}\n{\"type\":\"turn.completed\"}\n",
            b"{\"type\":\"thread.started\",\"thread_id\":\"native-observation-session\"}\n{\"type\":\"turn.started\"}\n",
        ),
        (
            "agy",
            b"{\"status\":\"SUCCESS\",\"conversation_id\":\"native-observation-session\"}\n",
            b"{\"conversation_id\":\"native-observation-session\"}\n",
        ),
    ]
}

fn observation(
    agent: &str,
) -> Result<(Arc<NativeSdkObservation>, Box<dyn PipedStdoutObserver>), EngineError> {
    let request = NativePrintRequest::new(agent, Some("native-observer"), None)?;
    let owner = Arc::new(NativeSdkObservation::new(request));
    let pipe = owner.stdout_observer();
    Ok((owner, pipe))
}

fn feed(pipe: &mut dyn PipedStdoutObserver, bytes: &[u8]) {
    for chunk in bytes.chunks(1) {
        pipe.observe(chunk);
    }
}

#[test]
fn canonical_acknowledgement_requires_clean_eof_and_returns_exact_session() -> TestResult {
    for (agent, complete, _) in cases() {
        let (owner, mut pipe) = observation(agent)?;
        assert!(owner.acknowledged_session().is_err());
        feed(pipe.as_mut(), complete);
        assert!(
            owner.acknowledged_session().is_err(),
            "{agent} acknowledged before clean stdout EOF"
        );
        pipe.finish(PipedReadEnd::Eof);
        assert_eq!(owner.acknowledged_session()?, SESSION);
    }
    Ok(())
}

#[test]
fn clean_eof_without_canonical_completion_cannot_acknowledge() -> TestResult {
    for (agent, _, incomplete) in cases() {
        for bytes in [b"".as_slice(), b"{\"message\":\"unrelated\"}\n", incomplete] {
            let (owner, mut pipe) = observation(agent)?;
            feed(pipe.as_mut(), bytes);
            pipe.finish(PipedReadEnd::Eof);
            assert!(
                owner.acknowledged_session().is_err(),
                "{agent} acknowledged missing completion: {bytes:?}"
            );
        }
    }
    Ok(())
}

#[test]
fn reader_and_parser_failures_remain_invalid_after_complete_prefix() -> TestResult {
    for (agent, complete, _) in cases() {
        for failure in [
            "reader-end",
            "reader-before-eof",
            "reader-after-eof",
            "malformed-tail",
            "truncated-tail",
        ] {
            let (owner, mut pipe) = observation(agent)?;
            feed(pipe.as_mut(), complete);
            match failure {
                "reader-end" => pipe.finish(PipedReadEnd::ReadError),
                "reader-before-eof" => {
                    owner.reader_failed();
                    pipe.finish(PipedReadEnd::Eof);
                }
                "reader-after-eof" => {
                    pipe.finish(PipedReadEnd::Eof);
                    assert_eq!(owner.acknowledged_session()?, SESSION);
                    owner.reader_failed();
                }
                "malformed-tail" => {
                    pipe.observe(b"!");
                    pipe.finish(PipedReadEnd::Eof);
                }
                "truncated-tail" => {
                    pipe.observe(b"{");
                    pipe.finish(PipedReadEnd::Eof);
                }
                _ => unreachable!("fixed failure case"),
            }
            assert!(
                owner.acknowledged_session().is_err(),
                "{agent} ignored {failure} after complete ACK bytes"
            );
            feed(pipe.as_mut(), complete);
            pipe.finish(PipedReadEnd::Eof);
            assert!(
                owner.acknowledged_session().is_err(),
                "{agent} cleared sticky {failure} after later valid input"
            );
        }
    }
    Ok(())
}
