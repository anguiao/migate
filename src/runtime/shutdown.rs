use crate::RuntimeError;

/// Register both interactive and service-manager shutdown signals before work starts.
pub async fn shutdown_signal() -> Result<&'static str, RuntimeError> {
    use async_signal::{Signal, Signals};
    use futures_lite::StreamExt;

    let mut signals = Signals::new([Signal::Int, Signal::Term])?;
    match signals
        .next()
        .await
        .ok_or("Shutdown signal stream ended unexpectedly")??
    {
        Signal::Int => Ok("Ctrl-C"),
        Signal::Term => Ok("SIGTERM"),
        _ => unreachable!("only shutdown signals are registered"),
    }
}
