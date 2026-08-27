# reporigor-process-tree

Internal process containment for reporigor runners.

`ProcessTree::spawn` creates a dedicated Unix process group or a Windows Job
Object. Every successful leader exit is followed by unconditional descendant
cleanup before the leader status is exposed. Timeouts, cancellation cleanup,
tree verification, and leader reaping are bounded by caller-provided durations;
cleanup failures are returned rather than silently discarded.

On Unix, `waitid(WNOWAIT)` observes leader exit without reaping it. Keeping the
leader identity alive prevents its PID/process-group ID from being reused before
the group has received its final cleanup signal.

This is execution containment, not a security sandbox. In particular, a
deliberately hostile Unix child can create a new session/process group and
escape its original group. Runners must execute trusted project toolchains; the
crate guarantees cleanup for descendants that remain in the inherited process
group.

On Windows, the child is created suspended, assigned to a Job Object configured
with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, and only then resumed. Rust's
`std::process::Command` does not expose the primary-thread handle, so the crate
locates the sole suspended initial thread with the Tool Help API, opens that
thread, and resumes it. Failure at any stage terminates the still-suspended
process with bounded cleanup and returns a `SpawnError`.

The small `unix` and `windows` modules contain the crate's only unsafe code.
Every FFI call documents its safety invariants locally; the workspace continues
to deny unsafe code everywhere else.
