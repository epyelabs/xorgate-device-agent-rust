//! The SMS-clear authorisation token (phase-5b §6 point 2; review-wp3 F1).
//!
//! The delete is reachable only through `crate::at_clear::clear_transport`,
//! which takes a [`ClearRequest`] BY VALUE. A `ClearRequest` can be made only
//! here — every field is private to this module, and there is no `Clone`,
//! `Copy` or `Default` — and only from a PROOF that one of the two designed
//! entry points has been passed, each proof consumed by the token it mints:
//!
//! - [`AcceptedClearSms`] — exists only for an `Effect::ClearSms`, which core
//!   `decide_command` emits only after the expiry check and the ledger claim
//!   (a QoS-1 redelivery is a ledger duplicate and yields no effect). Minted in
//!   the command plane's `Effects::run`, spent in its `run_sms`.
//! - [`CliConfirmation`] — exists only for `ClearGate::Proceed` (`--yes` /
//!   `-y`) or `ClearGate::Ask` with an answer `confirm_answer` accepts.
//!   Minted and spent in `cli::sms::clear_main`.
//!
//! So the COMPILER guarantees that nothing deletes without a token, and that
//! a token cannot be built outside this module or duplicated; a source test
//! (`xorgate-agent/tests/sms_clear_safety.rs`) pins the four constructor call
//! sites to those two functions.

use xorgate_core::command_flow::Effect;
use xorgate_core::sms_cli::{confirm_answer, ClearGate};

/// Proof of an accepted, ledger-claimed `clear_sms` (an `Effect::ClearSms`).
#[derive(Debug)]
pub struct AcceptedClearSms {
    _sealed: (),
}

impl AcceptedClearSms {
    /// `Some` only for an `Effect::ClearSms`.
    pub fn from_effect(e: &Effect) -> Option<Self> {
        matches!(e, Effect::ClearSms { .. }).then_some(Self { _sealed: () })
    }
}

/// Proof that `xorgate-rs sms clear` was confirmed.
#[derive(Debug)]
pub struct CliConfirmation {
    _sealed: (),
}

impl CliConfirmation {
    /// `Some` only for `--yes` (`Proceed`) or an interactive yes (`Ask` +
    /// an answer `/^y(es)?$/i` after trim); never for `Help` / `Refuse`.
    pub fn from_gate(gate: ClearGate, answer: Option<&str>) -> Option<Self> {
        let ok = match gate {
            ClearGate::Proceed => true,
            ClearGate::Ask => answer.is_some_and(confirm_answer),
            ClearGate::Help | ClearGate::Refuse => false,
        };
        ok.then_some(Self { _sealed: () })
    }
}

/// The token `clear_transport` (and `SmsReader::clear`) consume.
#[derive(Debug)]
pub struct ClearRequest {
    _sealed: (),
}

impl ClearRequest {
    /// From an accepted `clear_sms` (the proof is spent).
    pub fn accepted_command(proof: AcceptedClearSms) -> Self {
        let AcceptedClearSms { _sealed } = proof;
        Self { _sealed }
    }

    /// From a confirmed `xorgate-rs sms clear` (the proof is spent).
    pub fn confirmed_cli(proof: CliConfirmation) -> Self {
        let CliConfirmation { _sealed } = proof;
        Self { _sealed }
    }

    /// Spent by the delete itself (`at_clear::clear_transport`).
    pub(crate) fn spend(self) {
        let ClearRequest { _sealed } = self;
    }
}
