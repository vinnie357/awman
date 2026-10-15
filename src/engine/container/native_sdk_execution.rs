//! Prototype native publication; production seed and exact cleanup remain separate gates.

use std::sync::{Arc, Mutex};

use crate::data::fs::native_sdk_state::NativeSdkStateLease;
use crate::engine::agent::agent_matrix::{
    NativePrintRequest, NativeSdkAckEvent, NativeSdkAckFields,
};
use crate::engine::error::EngineError;

use super::io_bridge::{PipedReadEnd, PipedStdoutObserver};

pub(super) struct Observation {
    pub(super) session: Option<String>,
    pub(super) completed: bool,
    pub(super) peak_retained_bytes: usize,
}

pub(super) fn observe_sdk_chunks(
    agent: &str,
    resume: Option<&str>,
    chunks: &[&[u8]],
    clean_eof: bool,
) -> Result<Observation, EngineError> {
    let request = NativePrintRequest::new(agent, Some("native-observer"), resume)?;
    let mut observer = Observer::default();
    for chunk in chunks {
        observer.feed(chunk, &request)?;
    }
    if !clean_eof {
        return Err(invalid());
    }
    observer.end_input()?;
    Ok(Observation {
        session: observer.session,
        completed: observer.completed,
        peak_retained_bytes: observer.peak_retained_bytes,
    })
}

pub(super) struct NativeSdkExecution {
    request: NativePrintRequest,
    lease: NativeSdkStateLease,
    observer: Mutex<Observer>,
}

impl NativeSdkExecution {
    pub(super) fn new(request: NativePrintRequest, lease: NativeSdkStateLease) -> Self {
        Self {
            request,
            lease,
            observer: Mutex::new(Observer::default()),
        }
    }

    pub(super) fn validate(&self) -> Result<(), EngineError> {
        self.request.validate_state(&self.lease)
    }

    pub(super) fn stdout_observer(self: &Arc<Self>) -> Box<dyn PipedStdoutObserver> {
        Box::new(Arc::clone(self))
    }

    pub(super) fn reader_failed(&self) {
        if let Ok(mut observer) = self.observer.lock() {
            observer.invalid = true;
        }
    }

    pub(super) fn publish_after_release(&self) -> Result<(), EngineError> {
        self.validate()?;
        let observer = self.observer.lock().map_err(|_| invalid())?;
        if observer.invalid || !observer.eof {
            return Err(invalid());
        }
        if observer.session.is_some() && !observer.completed {
            return Err(invalid());
        }
        if observer.completed {
            let session = observer.session.as_deref().ok_or_else(invalid)?;
            self.lease.publish_session(session)?;
        }
        // Known quiescence releases the writer lock; failed state remains on disk.
        Ok(())
    }
}

impl PipedStdoutObserver for Arc<NativeSdkExecution> {
    fn observe(&mut self, bytes: &[u8]) {
        if let Ok(mut observer) = self.observer.lock() {
            if !observer.invalid && observer.feed(bytes, &self.request).is_err() {
                observer.invalid = true;
            }
        }
    }

    fn finish(&mut self, end: PipedReadEnd) {
        if let Ok(mut observer) = self.observer.lock() {
            if matches!(end, PipedReadEnd::Eof) && !observer.invalid && observer.end_input().is_ok()
            {
                observer.eof = true;
            } else {
                observer.invalid = true;
            }
        }
    }
}

fn invalid() -> EngineError {
    EngineError::Container(
        "native SDK output did not establish a complete session acknowledgement".into(),
    )
}

#[derive(Clone, Copy)]
enum Field {
    Type,
    Subtype,
    Status,
    Session,
    Thread,
    Conversation,
    Error,
    ProviderError,
}

impl Field {
    fn from_key(key: &str) -> Option<Self> {
        match key {
            "type" => Some(Self::Type),
            "subtype" => Some(Self::Subtype),
            "status" => Some(Self::Status),
            "session_id" => Some(Self::Session),
            "thread_id" => Some(Self::Thread),
            "conversation_id" => Some(Self::Conversation),
            "is_error" => Some(Self::Error),
            "error" => Some(Self::ProviderError),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
enum Stage {
    KeyOrEnd,
    Key,
    Colon,
    ValueOrEnd,
    Value,
    CommaOrEnd,
}

struct Frame {
    object: bool,
    stage: Stage,
    field: Option<Field>,
}

enum Token {
    String(StringToken),
    Number(Number),
    Literal {
        expected: &'static [u8],
        at: usize,
        boolean: Option<bool>,
    },
}

impl Token {
    fn retained_capacity_bytes(&self) -> usize {
        match self {
            Self::String(string) => string.retained_capacity_bytes(),
            Self::Number(_) | Self::Literal { .. } => 0,
        }
    }
}

#[derive(Clone, Copy)]
enum Number {
    Sign,
    Zero,
    Integer,
    Dot,
    Fraction,
    Exponent,
    ExponentSign,
    ExponentDigits,
}

impl Number {
    fn terminal(self) -> bool {
        matches!(
            self,
            Self::Zero | Self::Integer | Self::Fraction | Self::ExponentDigits
        )
    }

    fn consume(&mut self, byte: u8) -> bool {
        use Number::*;
        let next = match (*self, byte) {
            (Sign, b'0') => Zero,
            (Sign, b'1'..=b'9') => Integer,
            (Integer, b'0'..=b'9') => Integer,
            (Zero | Integer, b'.') => Dot,
            (Zero | Integer | Fraction, b'e' | b'E') => Exponent,
            (Dot | Fraction, b'0'..=b'9') => Fraction,
            (Exponent, b'+' | b'-') => ExponentSign,
            (Exponent | ExponentSign | ExponentDigits, b'0'..=b'9') => ExponentDigits,
            _ => return false,
        };
        *self = next;
        true
    }
}

#[derive(Clone, Copy)]
enum Escape {
    None,
    Slash,
    Unicode { value: u16, left: u8, low: bool },
    HighSlash,
    HighU,
}

struct StringToken {
    key: bool,
    field: Option<Field>,
    captured: Option<Vec<u8>>,
    escape: Escape,
    utf8: Vec<u8>,
    utf8_length: usize,
}

impl StringToken {
    fn retained_capacity_bytes(&self) -> usize {
        self.captured.as_ref().map_or(0, Vec::capacity) + self.utf8.capacity()
    }

    fn new(key: bool, field: Option<Field>) -> Self {
        Self {
            key,
            field,
            captured: (key || field.is_some()).then(|| vec![b'"']),
            escape: Escape::None,
            utf8: Vec::with_capacity(4),
            utf8_length: 0,
        }
    }

    fn consume(&mut self, byte: u8) -> Result<bool, EngineError> {
        if let Some(bytes) = self.captured.as_mut() {
            let bound = if self.key { 128 } else { 6146 };
            if bytes.len() == bound {
                if !self.key {
                    return Err(invalid());
                }
                self.captured = None;
            } else {
                bytes.push(byte);
            }
        }
        if !self.utf8.is_empty() {
            self.utf8.push(byte);
            if self.utf8.len() == self.utf8_length {
                std::str::from_utf8(&self.utf8).map_err(|_| invalid())?;
                self.utf8.clear();
            }
            return Ok(false);
        }
        match self.escape {
            Escape::Slash => match byte {
                b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {
                    self.escape = Escape::None
                }
                b'u' => {
                    self.escape = Escape::Unicode {
                        value: 0,
                        left: 4,
                        low: false,
                    }
                }
                _ => return Err(invalid()),
            },
            Escape::HighSlash if byte == b'\\' => self.escape = Escape::HighU,
            Escape::HighU if byte == b'u' => {
                self.escape = Escape::Unicode {
                    value: 0,
                    left: 4,
                    low: true,
                }
            }
            Escape::HighSlash | Escape::HighU => return Err(invalid()),
            Escape::Unicode { value, left, low } => {
                let digit = (byte as char).to_digit(16).ok_or_else(invalid)? as u16;
                let value = value * 16 + digit;
                if left > 1 {
                    self.escape = Escape::Unicode {
                        value,
                        left: left - 1,
                        low,
                    };
                } else if low {
                    if !(0xdc00..=0xdfff).contains(&value) {
                        return Err(invalid());
                    }
                    self.escape = Escape::None;
                } else if (0xd800..=0xdbff).contains(&value) {
                    self.escape = Escape::HighSlash;
                } else {
                    if (0xdc00..=0xdfff).contains(&value) {
                        return Err(invalid());
                    }
                    self.escape = Escape::None;
                }
            }
            Escape::None => match byte {
                b'"' => return Ok(true),
                b'\\' => self.escape = Escape::Slash,
                0..=31 => return Err(invalid()),
                0xc2..=0xdf => {
                    self.utf8.push(byte);
                    self.utf8_length = 2;
                }
                0xe0..=0xef => {
                    self.utf8.push(byte);
                    self.utf8_length = 3;
                }
                0xf0..=0xf4 => {
                    self.utf8.push(byte);
                    self.utf8_length = 4;
                }
                128..=255 => return Err(invalid()),
                _ => {}
            },
        }
        Ok(false)
    }

    fn decoded(&self) -> Result<Option<String>, EngineError> {
        self.captured
            .as_ref()
            .map(|bytes| serde_json::from_slice(bytes).map_err(|_| invalid()))
            .transpose()
    }
}

#[derive(Default)]
struct Observer {
    stack: Vec<Frame>,
    token: Option<Token>,
    fields: NativeSdkAckFields,
    selected_keys: u8,
    session: Option<String>,
    turn_started: bool,
    completed: bool,
    invalid: bool,
    eof: bool,
    peak_retained_bytes: usize,
}

impl Observer {
    fn record_peak(&mut self, local_token_capacity: usize) {
        let retained = self.stack.capacity() * std::mem::size_of::<Frame>()
            + self
                .token
                .as_ref()
                .map_or(0, Token::retained_capacity_bytes)
            + self.fields.retained_capacity_bytes()
            + self.session.as_ref().map_or(0, String::capacity)
            + local_token_capacity;
        self.peak_retained_bytes = self.peak_retained_bytes.max(retained);
    }

    fn field(&self) -> Option<Field> {
        (self.stack.len() == 1)
            .then(|| self.stack[0].field)
            .flatten()
            .filter(|field| !matches!(field, Field::ProviderError))
    }

    fn finish_value(&mut self) -> Result<(), EngineError> {
        let parent = self.stack.last_mut().ok_or_else(invalid)?;
        parent.stage = Stage::CommaOrEnd;
        parent.field = None;
        Ok(())
    }

    fn string_value(&mut self, string: StringToken) -> Result<(), EngineError> {
        let decoded = string.decoded()?;
        if string.key {
            let field = if self.stack.len() == 1 {
                decoded.as_deref().and_then(Field::from_key)
            } else {
                None
            };
            if let Some(field) = field {
                let bit = 1 << field as u8;
                if self.selected_keys & bit != 0 {
                    return Err(invalid());
                }
                self.selected_keys |= bit;
                if matches!(field, Field::ProviderError) {
                    self.fields.provider_error_present = true;
                }
            }
            let frame = self.stack.last_mut().ok_or_else(invalid)?;
            frame.field = field;
            frame.stage = Stage::Colon;
            self.record_peak(string.retained_capacity_bytes());
            return Ok(());
        }
        if let Some(field) = string.field {
            let value = decoded.ok_or_else(invalid)?;
            if value.len() > 1024 {
                return Err(invalid());
            }
            let slot = match field {
                Field::Type => &mut self.fields.event_type,
                Field::Subtype => &mut self.fields.subtype,
                Field::Status => &mut self.fields.status,
                Field::Session => &mut self.fields.session_id,
                Field::Thread => &mut self.fields.thread_id,
                Field::Conversation => &mut self.fields.conversation_id,
                Field::Error | Field::ProviderError => return Err(invalid()),
            };
            *slot = Some(value);
        }
        self.record_peak(string.retained_capacity_bytes());
        self.finish_value()
    }

    fn scalar_value(&mut self, boolean: Option<bool>) -> Result<(), EngineError> {
        if let Some(field) = self.field() {
            if !matches!(field, Field::Error) {
                return Err(invalid());
            }
            self.fields.is_error = Some(boolean.ok_or_else(invalid)?);
        }
        self.finish_value()
    }

    fn close(&mut self, object: bool, request: &NativePrintRequest) -> Result<(), EngineError> {
        let frame = self.stack.pop().ok_or_else(invalid)?;
        if frame.object != object {
            return Err(invalid());
        }
        if !self.stack.is_empty() {
            return self.finish_value();
        }
        let event = request.sdk_ack_event(std::mem::take(&mut self.fields))?;
        self.selected_keys = 0;
        match event {
            NativeSdkAckEvent::Session(id) => {
                if self.session.is_some() || self.turn_started || self.completed {
                    return Err(invalid());
                }
                self.session = Some(id);
            }
            NativeSdkAckEvent::CompletedSession(id) => {
                if self.session.as_ref().is_some_and(|old| old != &id) {
                    return Err(invalid());
                }
                self.completed = true;
                self.session = Some(id);
            }
            NativeSdkAckEvent::TurnStarted => {
                if self.session.is_none() || self.turn_started || self.completed {
                    return Err(invalid());
                }
                self.turn_started = true;
            }
            NativeSdkAckEvent::TurnCompleted => {
                if !self.turn_started || self.completed {
                    return Err(invalid());
                }
                self.completed = true;
            }
            NativeSdkAckEvent::Other => {}
        }
        self.record_peak(0);
        Ok(())
    }

    fn start_value(&mut self, byte: u8) -> Result<(), EngineError> {
        let field = self.field();
        match byte {
            b'{' | b'[' => {
                if field.is_some() || self.stack.len() == 64 {
                    return Err(invalid());
                }
                self.stack.push(Frame {
                    object: byte == b'{',
                    field: None,
                    stage: if byte == b'{' {
                        Stage::KeyOrEnd
                    } else {
                        Stage::ValueOrEnd
                    },
                });
            }
            b'"' => self.token = Some(Token::String(StringToken::new(false, field))),
            b'-' => self.token = Some(Token::Number(Number::Sign)),
            b'0' => self.token = Some(Token::Number(Number::Zero)),
            b'1'..=b'9' => self.token = Some(Token::Number(Number::Integer)),
            b't' => {
                self.token = Some(Token::Literal {
                    expected: b"true",
                    at: 1,
                    boolean: Some(true),
                })
            }
            b'f' => {
                self.token = Some(Token::Literal {
                    expected: b"false",
                    at: 1,
                    boolean: Some(false),
                })
            }
            b'n' => {
                self.token = Some(Token::Literal {
                    expected: b"null",
                    at: 1,
                    boolean: None,
                })
            }
            _ => return Err(invalid()),
        }
        self.record_peak(0);
        Ok(())
    }

    fn feed(&mut self, bytes: &[u8], request: &NativePrintRequest) -> Result<(), EngineError> {
        for &byte in bytes {
            self.record_peak(0);
            match self.token.take() {
                Some(Token::String(mut string)) => {
                    let complete = string.consume(byte)?;
                    self.record_peak(string.retained_capacity_bytes());
                    if complete {
                        self.string_value(string)?;
                    } else {
                        self.token = Some(Token::String(string));
                    }
                    continue;
                }
                Some(Token::Literal {
                    expected,
                    mut at,
                    boolean,
                }) => {
                    if expected.get(at) != Some(&byte) {
                        return Err(invalid());
                    }
                    at += 1;
                    if at == expected.len() {
                        self.scalar_value(boolean)?;
                    } else {
                        self.token = Some(Token::Literal {
                            expected,
                            at,
                            boolean,
                        });
                    }
                    continue;
                }
                Some(Token::Number(mut number)) => {
                    if number.consume(byte) {
                        self.token = Some(Token::Number(number));
                        continue;
                    }
                    if !number.terminal() {
                        return Err(invalid());
                    }
                    self.scalar_value(None)?;
                }
                None => {}
            }
            if matches!(byte, b' ' | b'\r' | b'\n' | b'\t') {
                continue;
            }
            let Some(frame) = self.stack.last() else {
                if byte != b'{' {
                    return Err(invalid());
                }
                self.start_value(byte)?;
                continue;
            };
            let object = frame.object;
            match frame.stage {
                Stage::KeyOrEnd if byte == b'}' => self.close(true, request)?,
                Stage::KeyOrEnd | Stage::Key if byte == b'"' => {
                    self.token = Some(Token::String(StringToken::new(true, None)));
                }
                Stage::Colon if byte == b':' => {
                    self.stack.last_mut().ok_or_else(invalid)?.stage = Stage::Value
                }
                Stage::ValueOrEnd if byte == b']' => self.close(false, request)?,
                Stage::ValueOrEnd | Stage::Value => self.start_value(byte)?,
                Stage::CommaOrEnd if byte == b',' => {
                    self.stack.last_mut().ok_or_else(invalid)?.stage =
                        if object { Stage::Key } else { Stage::Value };
                }
                Stage::CommaOrEnd if (object && byte == b'}') || (!object && byte == b']') => {
                    self.close(object, request)?
                }
                _ => return Err(invalid()),
            }
            self.record_peak(0);
        }
        Ok(())
    }

    fn end_input(&mut self) -> Result<(), EngineError> {
        self.record_peak(0);
        if self.token.is_some() || !self.stack.is_empty() {
            return Err(invalid());
        }
        Ok(())
    }
}
