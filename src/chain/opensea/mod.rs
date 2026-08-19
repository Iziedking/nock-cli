//! Everything that talks to `OpenSea`, and nothing else does.
//!
//! `OpenSea` is an untrusted supplier of the one thing this tool cannot compute
//! for itself: the signature a signed stage requires. Keeping every request,
//! type and response behind this boundary means the rest of the CLI never sees
//! a GraphQL shape, and a change on their side lands in one directory with
//! recorded fixtures around it rather than spreading through the mint path.
//!
//! Built ahead of its callers. The planner and the mint command are what finally
//! consume this, so until those land the compiler is right that it is unused.
//! Allowed once here rather than per item, so the exception is a single decision
//! in a single place and can be deleted in one edit when the callers arrive.
#![allow(dead_code)]

pub mod siwe;
