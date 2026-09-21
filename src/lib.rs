//! augur suggests how the command line being typed continues, one word at a
//! time, from the commands you ran before, the context you ran them in, your
//! shell's completion and, as a last resort, a local language model.
//!
//! The shell integration (`shell/augur.bash`, for ble.sh) records every
//! executed command to the [`history`] log and asks `augur serve` for a
//! suggestion over the [`protocol`] while you type. [`suggest`] decides what to
//! suggest, from the [`tokens`] model, the shell's completion, [`fuzzy`]
//! matching and the [`llm`]; [`eval`] measures it against ble.sh's own history
//! suggestions.

pub mod commands;
pub mod escape;
pub mod eval;
pub mod files;
pub mod fuzzy;
pub mod history;
pub mod lexer;
pub mod llm;
pub mod protocol;
pub mod repo;
pub mod state;
pub mod suggest;
pub mod tokens;
