//! Mounts the LLM as a kernel service. Today it's the DeepSeek adapter; a
//! different provider is a different plugin providing the same "llm" key.

use std::sync::Arc;

use crate::harness::{Agente, Kernel, Plugin};
use crate::llm::Llm;

pub struct LlmAdapter {
    pub llm: Arc<Llm>,
}

impl Plugin<Agente> for LlmAdapter {
    fn name(&self) -> &'static str {
        "llm-deepseek"
    }
    fn apply(&self, k: &mut Kernel) -> anyhow::Result<()> {
        k.provide("llm", self.llm.clone());
        Ok(())
    }
}
