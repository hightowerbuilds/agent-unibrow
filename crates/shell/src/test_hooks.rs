//! Test hooks: environment variables the eval runner sets to drive the app
//! (send a task, answer approval cards, finish handoffs, run pane steps).
//!
//! They only work in isolated test instances (`$UNIBROW_DATA_DIR` set), so
//! an ordinary launch can never be made to approve or send on its own.
//! Read every hook through [`var`].

/// The hook's value in a test instance; `None` otherwise.
pub(crate) fn var(name: &str) -> Option<String> {
    std::env::var_os("UNIBROW_DATA_DIR")?;
    std::env::var(name).ok()
}
