use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct LatencyMetrics {
    pub message_received: Instant,
    pub trigger_evaluation_started: Instant,
    pub trigger_validated: Instant,
    pub trigger_acquired: Instant,
    pub finalization_started: Option<Instant>,
    pub finalization_completed: Option<Instant>,
    pub signing_started: Option<Instant>,
    pub signing_completed: Option<Instant>,
    pub broadcast_started: Option<Instant>,
    pub first_rpc_response: Option<Instant>,
}

impl LatencyMetrics {
    pub fn new(message_received: Instant) -> Self {
        Self {
            message_received,
            trigger_evaluation_started: message_received,
            trigger_validated: message_received,
            trigger_acquired: message_received,
            finalization_started: None,
            finalization_completed: None,
            signing_started: None,
            signing_completed: None,
            broadcast_started: None,
            first_rpc_response: None,
        }
    }

    pub fn local_critical_path(&self) -> Option<Duration> {
        self.broadcast_started
            .map(|started| started.saturating_duration_since(self.trigger_acquired))
    }

    pub fn print(&self) {
        let elapsed_ms = |end: Instant, start: Instant| {
            end.saturating_duration_since(start).as_secs_f64() * 1_000.0
        };
        println!("\nLATENCY REPORT");
        println!("--------------------------------");
        println!(
            "Trigger evaluation       {:.3} ms",
            elapsed_ms(self.trigger_validated, self.trigger_evaluation_started)
        );
        println!(
            "Atomic state transition  {:.3} ms",
            elapsed_ms(self.trigger_acquired, self.trigger_validated)
        );
        if let (Some(start), Some(end)) = (self.finalization_started, self.finalization_completed) {
            println!("Transaction finalization {:.3} ms", elapsed_ms(end, start));
        }
        if let (Some(start), Some(end)) = (self.signing_started, self.signing_completed) {
            println!("Signing                  {:.3} ms", elapsed_ms(end, start));
        }
        if let (Some(start), Some(end)) = (self.signing_completed, self.broadcast_started) {
            println!("Local sign → send        {:.3} ms", elapsed_ms(end, start));
        }
        if let (Some(start), Some(end)) = (self.broadcast_started, self.first_rpc_response) {
            println!("RPC submission           {:.3} ms", elapsed_ms(end, start));
        }
        if let Some(total) = self.local_critical_path() {
            println!(
                "\nLocal critical path      {:.3} ms",
                total.as_secs_f64() * 1_000.0
            );
            if let (Some(start), Some(end)) = (self.broadcast_started, self.first_rpc_response) {
                println!("RPC/network latency      {:.3} ms", elapsed_ms(end, start));
            }
        }
        if let Some(response) = self.first_rpc_response {
            println!(
                "Trigger → acknowledgement {:.3} ms",
                elapsed_ms(response, self.message_received)
            );
        }
    }
}
