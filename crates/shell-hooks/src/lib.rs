//! Managed provider hook generation and normalized lifecycle events.
//! Session-scoped provider hooks and an authenticated, transport-neutral receiver.
//! See the crate README for the HTTP contract and provider support boundaries.

mod generate;
mod receiver;

pub use generate::{GeneratedHooks, GenerationOptions, generate, shell_quote};
pub use receiver::{
    AcceptedEvent, Binding, HookEnvelope, HookReceiver, Limits, Outcome, Peer, Provider,
    Registration, TurnEvent, normalize,
};
