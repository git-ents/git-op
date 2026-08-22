//! An operation log for Git refs, configurations, and descriptions.
//!
//! Git's `reference-transaction` hook receives every Git ref update before and after it's written.
//! We can use this functionality to record the full mutable repository state --- all refs --- in Git.
//! A special ref, exempted from the operation log, is used to store the object ID (OID) associated with every local ref, on every committed `reference-transaction` hook invocation.
//! Remote references, e.g. `refs/remotes/origin/main`, are exempted.
//! Local symbolic refs that are specially handled by the Git CLI, e.g. `HEAD`, are also exempted.
