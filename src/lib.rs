//! unmcpk — fast, fully automated unpacker for NetEase MCPK containers.
//!
//! The library is split along the layers of the format, from the outside in:
//!
//! * [`mcpk`] — the container (header, directory table, file index, bodies).
//! * [`payload`] — the obfuscation around each body (rotor, zlib, reversal) plus
//!   the `StringID` hash the container uses for names.
//! * [`rotor`] — the rotor machine itself.
//! * [`nemarshal`] — the mutated Python 2.7 marshal reader (tags, encryption,
//!   shuffled code layouts, permuted opcodes).
//! * [`pyc`] — emitter for plain CPython 2.7 pyc files.
//! * [`pathmap`] — rebuilds the original source tree from `co_filename`.
//! * [`decompile`] — pluggable decompiler backends.
//! * [`pipeline`] — the two-phase orchestrator used by the CLI.

pub mod decompile;
pub mod mcpk;
pub mod nemarshal;
pub mod pathmap;
pub mod payload;
pub mod pyc;
pub mod pipeline;
pub mod rotor;
