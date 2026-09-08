// Wiring lands in a later task (tracing setup, the signal-cli client, the
// dialog loop). Nothing here yet on purpose: no prose belongs in `main`
// outside `tracing`, and `tracing`'s own startup line is that later task's
// to write.
fn main() {}
