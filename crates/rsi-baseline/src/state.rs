//! Consuming, forward-only producer state.  It deliberately performs no I/O.
#![allow(
    clippy::expect_used,
    clippy::missing_const_for_fn,
    clippy::missing_panics_doc
)]

use core::marker::PhantomData;

use crate::{bounds::BOUNDS_V1, wjr::TerminalFactV2};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dormant;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WitnessValidated;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthorityConstructed;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Executing;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandidateReady;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Published;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rejected;

/// A producer stage can only advance by consuming its predecessor.
#[derive(Debug, PartialEq, Eq)]
pub struct Producer<S> {
    commands: u64,
    terminal: Option<TerminalFactV2>,
    marker: PhantomData<S>,
}

impl Producer<Dormant> {
    #[must_use]
    pub const fn dormant() -> Self {
        Self {
            commands: 0,
            terminal: None,
            marker: PhantomData,
        }
    }
    #[must_use]
    pub fn validate_witness(self) -> Producer<WitnessValidated> {
        self.into_state()
    }
}

impl Producer<WitnessValidated> {
    #[must_use]
    pub fn construct_authority(self) -> Producer<AuthorityConstructed> {
        self.into_state()
    }
}

impl Producer<AuthorityConstructed> {
    #[must_use]
    pub fn begin_execution(self) -> Producer<Executing> {
        self.into_state()
    }
}

impl Producer<Executing> {
    /// Advances the private monotonic command counter or consumes executable
    /// authority into the supplied first-red terminal state.
    pub fn next_command(mut self, first_red: TerminalFactV2) -> Result<Self, Producer<Rejected>> {
        if let Some(next) = self.commands.checked_add(1)
            && next <= BOUNDS_V1.commands
        {
            self.commands = next;
            return Ok(self);
        }
        Err(self.reject(first_red))
    }
    #[must_use]
    pub const fn command_count(&self) -> u64 {
        self.commands
    }
    #[must_use]
    pub fn candidate_ready(self) -> Producer<CandidateReady> {
        self.into_state()
    }
}

impl Producer<CandidateReady> {
    #[must_use]
    pub fn publish(self) -> Producer<Published> {
        self.into_state()
    }
}

macro_rules! rejectable {
    ($state:ty) => {
        impl Producer<$state> {
            #[must_use]
            pub fn reject(self, terminal: TerminalFactV2) -> Producer<Rejected> {
                Producer {
                    commands: self.commands,
                    terminal: Some(terminal),
                    marker: PhantomData,
                }
            }
        }
    };
}
rejectable!(Dormant);
rejectable!(WitnessValidated);
rejectable!(AuthorityConstructed);
rejectable!(Executing);
rejectable!(CandidateReady);

impl Producer<Rejected> {
    /// Consume the latched first-red reason for terminal WJR construction.
    #[must_use]
    pub fn into_terminal_fact(self) -> TerminalFactV2 {
        self.terminal
            .expect("rejected producer always has a terminal fact")
    }
}

impl<S> Producer<S> {
    fn into_state<T>(self) -> Producer<T> {
        Producer {
            commands: self.commands,
            terminal: self.terminal,
            marker: PhantomData,
        }
    }
}

/// ```compile_fail
/// use rsi_baseline::{Producer, Dormant};
/// let _ = Producer::<Dormant>::dormant().publish();
/// ```
/// ```compile_fail
/// use rsi_baseline::{Producer, Dormant};
/// let _ = Producer::<Dormant>::dormant().reject();
/// ```
/// ```compile_fail
/// use rsi_baseline::{Producer, Dormant};
/// let p = Producer::<Dormant>::dormant().validate_witness().construct_authority().begin_execution().candidate_ready().publish();
/// let _ = p.reject;
/// ```
/// ```compile_fail
/// use rsi_baseline::{Producer, Dormant};
/// let p = Producer::<Dormant>::dormant().validate_witness();
/// let _ = p.validate_witness();
/// ```
/// ```compile_fail
/// use rsi_baseline::{Producer, Dormant};
/// let p = Producer::<Dormant>::dormant();
/// let _ = p.clone();
/// ```
/// ```compile_fail
/// use rsi_baseline::{Producer, Dormant};
/// let p = Producer::<Dormant>::dormant().validate_witness().construct_authority().begin_execution().candidate_ready().publish();
/// let _ = p.validate_witness();
/// ```
/// ```compile_fail
/// use rsi_baseline::{Producer, Rejected};
/// let _ = Producer::<Rejected>::dormant();
/// ```
/// ```compile_fail
/// use rsi_baseline::{Producer, Dormant, TerminalFactV2};
/// fn reason() -> TerminalFactV2 { loop {} }
/// let executing = Producer::<Dormant>::dormant().validate_witness().construct_authority().begin_execution();
/// let _result = executing.next_command(reason());
/// let _ = executing.command_count();
/// ```
/// ```compile_fail
/// use rsi_baseline::{Producer, Dormant, TerminalFactV2};
/// fn reason() -> TerminalFactV2 { loop {} }
/// let rejected = Producer::<Dormant>::dormant().validate_witness().construct_authority().begin_execution().next_command(reason()).unwrap_err();
/// let _ = rejected.next_command(reason());
/// ```
/// ```compile_fail
/// use rsi_baseline::{Producer, Dormant, TerminalFactV2};
/// fn reason() -> TerminalFactV2 { loop {} }
/// let rejected = Producer::<Dormant>::dormant().validate_witness().construct_authority().begin_execution().next_command(reason()).unwrap_err();
/// let _ = rejected.candidate_ready();
/// ```
/// ```compile_fail
/// use rsi_baseline::{Producer, Dormant, TerminalFactV2};
/// fn reason() -> TerminalFactV2 { loop {} }
/// let rejected = Producer::<Dormant>::dormant().validate_witness().construct_authority().begin_execution().next_command(reason()).unwrap_err();
/// let _ = rejected.publish();
/// ```
/// ```compile_fail
/// use rsi_baseline::{Producer, Dormant, TerminalFactV2};
/// fn reason() -> TerminalFactV2 { loop {} }
/// let rejected = Producer::<Dormant>::dormant().validate_witness().construct_authority().begin_execution().next_command(reason()).unwrap_err();
/// let _ = rejected.reject(reason());
/// ```
const _: () = ();
