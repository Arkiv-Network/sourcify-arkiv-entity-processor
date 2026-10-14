//! Request-owned RPC dispatch trace. No recorder is created for ordinary reads.
use serde::Serialize;
use serde_json::Value;
use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
    time::Instant,
};

const MAX_CALLS: usize = 4_096;
const MAX_BYTES: usize = 2 * 1024 * 1024;

/// Opaque optional context passed to transports by the debug endpoint.
#[derive(Clone, Default)]
pub struct RpcTrace(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    calls: Vec<RpcCall>,
    bytes: usize,
    total: u64,
    omitted: u64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RpcCall {
    index: u64,
    network: &'static str,
    method: String,
    params: Value,
    duration_ms: u64,
    outcome: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Snapshot {
    rpc_calls: Vec<RpcCall>,
    truncated: bool,
    omitted_rpc_calls: u64,
}

#[derive(Serialize)]
pub(crate) struct DebugResponse<T> {
    pub response: T,
    pub trace: Snapshot,
}

// Count the encoded parameters without allocating a second JSON buffer. The
// fixed allowance covers entry keys, punctuation, method/network and u64 fields.
struct ByteBudget(usize);
impl Write for ByteBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("trace byte limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl RpcTrace {
    /// Called after admission/budget checks, immediately before sending HTTP.
    /// The vector records dispatch order, independent of completion order.
    pub(crate) fn start(&self, network: &'static str, method: &str, params: &Value) -> Call {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let index = state.total;
        state.total += 1;
        let allowance = MAX_BYTES.saturating_sub(state.bytes);
        let mut budget = ByteBudget(allowance);
        // Once truncated, retain a complete prefix; later, smaller calls must
        // not make a gapped trace look complete.
        let slot = if state.omitted == 0
            && state.calls.len() < MAX_CALLS
            && budget.write_all(&[0; 256]).is_ok()
            && serde_json::to_writer(&mut budget, method).is_ok()
            && serde_json::to_writer(&mut budget, params).is_ok()
        {
            state.bytes += allowance - budget.0;
            let slot = state.calls.len();
            state.calls.push(RpcCall {
                index,
                network,
                method: method.to_owned(),
                params: params.clone(),
                duration_ms: 0,
                outcome: "cancelled",
            });
            Some(slot)
        } else {
            state.omitted += 1;
            None
        };
        Call {
            trace: self.clone(),
            slot,
            started: Instant::now(),
            outcome: "cancelled",
        }
    }

    /// Snapshot after RPC futures finish or are dropped. Keep the bounded
    /// original intact so a deadline during serialization can still return it.
    pub(crate) fn snapshot(&self) -> Snapshot {
        let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        Snapshot {
            rpc_calls: state.calls.clone(),
            truncated: state.omitted > 0,
            omitted_rpc_calls: state.omitted,
        }
    }
}

pub(crate) struct Call {
    trace: RpcTrace,
    slot: Option<usize>,
    started: Instant,
    outcome: &'static str,
}
impl Call {
    pub(crate) fn finish(mut self, success: bool) {
        self.outcome = if success { "success" } else { "error" };
    }
}
impl Drop for Call {
    fn drop(&mut self) {
        if let Some(slot) = self.slot {
            let mut state = self.trace.0.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(call) = state.calls.get_mut(slot) {
                call.duration_ms = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                call.outcome = self.outcome;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dispatch_order_survives_out_of_order_completion_and_cancellation() {
        let trace = RpcTrace::default();
        let first = trace.start("arkiv", "arkiv_query", &json!(["first"]));
        let second = trace.start("ethereum", "eth_call", &json!(["second"]));
        second.finish(false);
        drop(first);
        let snapshot = serde_json::to_value(trace.snapshot()).unwrap();
        assert_eq!(snapshot["rpcCalls"][0]["index"], 0);
        assert_eq!(snapshot["rpcCalls"][0]["outcome"], "cancelled");
        assert_eq!(snapshot["rpcCalls"][1]["index"], 1);
        assert_eq!(snapshot["rpcCalls"][1]["outcome"], "error");
        assert_eq!(snapshot["truncated"], false);
    }

    #[test]
    fn count_and_byte_limits_keep_a_bounded_prefix_and_count_omissions() {
        let trace = RpcTrace::default();
        for _ in 0..MAX_CALLS + 3 {
            trace.start("arkiv", "eth_chainId", &json!([])).finish(true);
        }
        let snapshot = trace.snapshot();
        assert_eq!(snapshot.rpc_calls.len(), MAX_CALLS);
        assert_eq!(snapshot.omitted_rpc_calls, 3);
        assert!(snapshot.truncated);
        assert!(serde_json::to_vec(&snapshot).unwrap().len() < MAX_BYTES);

        let trace = RpcTrace::default();
        trace.start("arkiv", "eth_chainId", &json!([])).finish(true);
        trace
            .start("arkiv", "arkiv_query", &json!(["x".repeat(MAX_BYTES)]))
            .finish(true);
        trace.start("arkiv", "eth_chainId", &json!([])).finish(true);
        let snapshot = trace.snapshot();
        assert_eq!(snapshot.rpc_calls.len(), 1);
        assert_eq!(snapshot.omitted_rpc_calls, 2);
        assert!(snapshot.truncated);
    }
}
