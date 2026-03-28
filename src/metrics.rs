#[derive(Debug, Default)]
pub struct RunMetrics {
    pub total_tool_calls:        u32,
    pub successful_tool_calls:   u32,
    pub failed_tool_calls:       u32,
    pub retried_tool_calls:      u32,
    pub model_calls:            u32,
    pub model_retries:          u32,
    pub total_model_latency_ms: u64,
    pub total_tool_latency_ms:   u64,
    pub urls_fetched:            usize,
    pub estimated_tokens_used:   usize,
}

impl RunMetrics {
    pub fn record_tool_success(&mut self, duration_ms: u64) {
        self.total_tool_calls      += 1;
        self.successful_tool_calls += 1;
        self.total_tool_latency_ms += duration_ms;
    }

    pub fn record_tool_failure(&mut self, duration_ms: u64) {
        self.total_tool_calls    += 1;
        self.failed_tool_calls   += 1;
        self.total_tool_latency_ms += duration_ms;
    }

    pub fn record_model_call(&mut self, duration_ms: u64, retries: u32) {
        self.model_calls            += 1;
        self.model_retries          += retries;
        self.total_model_latency_ms += duration_ms;
    }

    pub fn avg_model_latency_ms(&self) -> u64 {
        if self.model_calls == 0 { return 0; }
        self.total_model_latency_ms / self.model_calls as u64
    }

    pub fn tool_success_rate(&self) -> f32 {
        if self.total_tool_calls == 0 { return 1.0; }
        self.successful_tool_calls as f32 / self.total_tool_calls as f32
    }
}