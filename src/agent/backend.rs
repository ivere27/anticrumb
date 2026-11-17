//! Model transport contract for the agent loop.
pub trait AgentBackend: Send {
    fn set_events(&mut self, _events: super::EventSink) {}
    fn set_cancel(&mut self, _cancel: super::CancelToken) {}
    fn retry_safe(&self) -> bool {
        false
    }
    fn add_artifacts(&mut self, artifacts: &[String]) -> Result<(), String> {
        if artifacts.is_empty() {
            Ok(())
        } else {
            Err("backend does not support attachments".into())
        }
    }
    /// Replace ephemeral tool images for the next model call, without retaining user files.
    fn set_step_artifacts(&mut self, artifacts: &[String]) -> Result<(), String> {
        if artifacts.is_empty() {
            Ok(())
        } else {
            Err("backend does not support tool images".into())
        }
    }
    fn reset_session(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn ask_step(
        &mut self,
        prompt: &str,
        is_first_step: bool,
        on_chunk: &mut dyn FnMut(&str),
    ) -> Result<String, String>;
}
