use std::time::{Duration, Instant};

use super::approval::PermissionDecision;
use super::backend::AgentBackend;
use super::events::{
    AgentConfig, AgentEvent, AgentRunResult, AgentStatus, AgentStepRecord, OutputStream,
    RunControl, ToolCallRequest, ToolOutput,
};
use super::parser::parse_response;
use super::prompts::{LoadedPrompts, PromptContext, DEFAULT_AST_NOTE, DEFAULT_SUBAGENT_NOTE};
use super::tools::{BrowserControlFn, BrowserFetchFn, SpawnAgentFn, ToolContext, ToolRegistry};
use super::transcript::Transcript;

/// Consecutive replies with an unparsable tool envelope before the run is aborted.
const MAX_MALFORMED_STREAK: u32 = 3;
/// Identical consecutive tool-call sets before the model gets an explicit "you are looping" hint.
const REPEAT_WARN_THRESHOLD: u32 = 3;

pub(crate) const SHELL_TOOLS: [&str; 4] = [
    "run_shell",
    "start_process",
    "process_output",
    "stop_process",
];

/// ReAct style coding agent: ask the model, run the tools it requests (read-only ones in
/// parallel), feed observations back, repeat until it answers without tool calls.
///
/// The loop is blocking by design (one CDP round-trip per step) but fully controllable from the
/// outside through [`RunControl`]: cancel it, steer it with new user messages, and observe every
/// step as an [`AgentEvent`]. The workspace host runs it on a supervised blocking worker.
pub struct CodingAgent<B: AgentBackend> {
    backend: B,
    registry: ToolRegistry,
    config: AgentConfig,
    browser_fetch: Option<BrowserFetchFn>,
    browser_control: Option<BrowserControlFn>,
    spawn_agent: Option<SpawnAgentFn>,
}

impl<B: AgentBackend> CodingAgent<B> {
    pub fn new(backend: B, config: AgentConfig) -> Self {
        let mut registry = ToolRegistry::with_builtin_coding_tools();
        if !config.allow_shell {
            for name in SHELL_TOOLS {
                registry.remove(name);
            }
        }
        if config.temp_chat {
            registry.remove("spawn_agent");
        }
        Self::with_registry(backend, registry, config)
    }

    pub fn with_registry(backend: B, registry: ToolRegistry, config: AgentConfig) -> Self {
        Self {
            backend,
            registry,
            config,
            browser_fetch: None,
            browser_control: None,
            spawn_agent: None,
        }
    }

    pub fn with_browser_fetch(mut self, fetcher: BrowserFetchFn) -> Self {
        self.browser_fetch = Some(fetcher);
        self
    }

    pub fn with_browser_control(mut self, controller: BrowserControlFn) -> Self {
        self.browser_control = Some(controller);
        self
    }

    pub fn with_spawn_agent(mut self, runner: SpawnAgentFn) -> Self {
        self.spawn_agent = Some(runner);
        self
    }

    pub fn build_initial_prompt(&self, user_task: &str) -> String {
        if self.registry.is_empty() {
            return user_task.to_string();
        }
        let workspace_for_instructions = self
            .config
            .load_instructions
            .then_some(self.config.workspace_root.as_path());
        let prompts = LoadedPrompts::load(
            self.config.prompts_dir.as_deref(),
            workspace_for_instructions,
        );
        let tools_block = self
            .registry
            .render_compact_tools_block(&prompts.tool_descriptions);
        let ast_note = if self.registry.contains("code_outline") {
            DEFAULT_AST_NOTE
        } else {
            ""
        };
        let subagent_note = if self.registry.contains("spawn_agent") {
            DEFAULT_SUBAGENT_NOTE
        } else {
            ""
        };
        let shell_note = if self.config.allow_shell {
            " with run_shell (use start_process for long-running processes)"
        } else {
            " (shell access is disabled)"
        };
        prompts.render_initial_prompt(&PromptContext {
            workspace_root: &self.config.workspace_root,
            ast_note,
            shell_note,
            subagent_note,
            max_calls: self.config.max_tool_calls_per_step.max(1),
            tools_block: &tools_block,
            user_task,
        })
    }

    fn result(
        &self,
        scope: &RunScope<'_>,
        status: AgentStatus,
        final_response: String,
        steps: Vec<AgentStepRecord>,
    ) -> AgentRunResult {
        let result = AgentRunResult {
            task: scope.task.to_string(),
            status,
            final_response,
            steps,
            files_changed: scope.ctx.changed_files(),
            temp_chat: self.config.temp_chat,
            elapsed_ms: scope.start.elapsed().as_millis(),
            inspector_pruned: false,
        };
        scope.control.events.emit(AgentEvent::Finished {
            result: result.clone(),
        });
        result
    }

    /// Run the agent to completion (or cancellation / step budget). `Err` means the backend kept
    /// failing; cancellation is reported as `Ok` with [`AgentStatus::Cancelled`].
    pub fn run(&mut self, task: &str, control: &RunControl) -> Result<AgentRunResult, String> {
        let start = Instant::now();
        self.backend.set_cancel(control.cancel.clone());
        self.backend.set_events(control.events.clone());
        let ev = &control.events;
        let max_steps = if self.config.max_steps == 0 {
            usize::MAX
        } else {
            self.config.max_steps
        };
        let max_calls = self.config.max_tool_calls_per_step.max(1);

        ev.emit(AgentEvent::Started {
            task: task.to_string(),
            workspace: self.config.workspace_root.display().to_string(),
            max_steps,
            temp_chat: self.config.temp_chat,
        });

        let mut ctx = ToolContext::new(self.config.workspace_root.clone());
        ctx.environment = self.config.environment.clone();
        ctx.shell_timeout = self.config.shell_timeout;
        ctx.allow_shell = self.config.allow_shell;
        ctx.cancel = control.cancel.clone();
        ctx.browser_fetch = self.browser_fetch.clone();
        ctx.browser_control = self.browser_control.clone();
        ctx.spawn_agent = self.spawn_agent.clone();
        ctx.dedup_reads = !self.config.temp_chat;
        let scope = RunScope {
            task,
            start,
            control,
            ctx: &ctx,
        };

        let initial = self.build_initial_prompt(task);
        let mut transcript = Transcript::new(initial.clone());
        // User-side parts to send with the next model call (persistent chat sends only these).
        let mut outgoing: Vec<String> = vec![initial];
        let mut steps: Vec<AgentStepRecord> = Vec::new();
        let mut malformed_streak = 0u32;
        let mut last_signature: Vec<String> = Vec::new();
        let mut repeat_count = 0u32;
        let mut last_response = String::new();

        for step in 1..=max_steps {
            if control.cancel.is_cancelled() {
                return Ok(self.result(&scope, AgentStatus::Cancelled, last_response, steps));
            }
            let mut notes: Vec<String> = Vec::new();

            for msg in control.inbox.drain() {
                self.backend.add_artifacts(&msg.artifacts)?;
                let msg = msg.message;
                ev.emit(AgentEvent::Steered {
                    message: msg.clone(),
                });
                notes.push(format!("user message: {msg}"));
                let part = format!("[USER_MESSAGE]\n{msg}");
                transcript.push_user(part.clone(), false);
                outgoing.push(part);
            }

            ev.emit(AgentEvent::StepStarted { step });
            // A persistent session already contains prior turns. Send only the new
            // material; the backend can attach large prompts without resetting it.
            let prompt = if self.config.temp_chat && step > 1 {
                transcript.render(self.config.context_budget_chars)
            } else {
                outgoing.join("\n\n")
            };
            if self.config.temp_chat && prompt.chars().count() > self.config.context_budget_chars {
                return Err("context budget exceeded after compaction; shorten the task or tool results (user instructions were preserved)".into());
            }

            // ---- model call (with retries) ----------------------------------------------
            let mut attempt = 0u32;
            let response = loop {
                let mut on_chunk = |chunk: &str| {
                    ev.emit(AgentEvent::ModelChunk {
                        step,
                        text: chunk.to_string(),
                    })
                };
                match self.backend.ask_step(&prompt, step == 1, &mut on_chunk) {
                    Ok(r) => break r,
                    Err(err) => {
                        if control.cancel.is_cancelled() {
                            return Ok(self.result(
                                &scope,
                                AgentStatus::Cancelled,
                                last_response,
                                steps,
                            ));
                        }
                        attempt += 1;
                        if !self.backend.retry_safe() || attempt > self.config.step_retries {
                            return Err(format!(
                                "step {step} failed after {attempt} attempt(s): {err}"
                            ));
                        }
                        ev.emit(AgentEvent::Retrying {
                            step,
                            attempt,
                            error: err,
                        });
                        if control
                            .cancel
                            .sleep_or_cancelled(Duration::from_millis(500 * u64::from(attempt)))
                        {
                            return Ok(self.result(
                                &scope,
                                AgentStatus::Cancelled,
                                last_response,
                                steps,
                            ));
                        }
                    }
                }
            };
            outgoing.clear();
            last_response = response.clone();
            ev.emit(AgentEvent::ModelResponse {
                step,
                text: response.clone(),
            });
            transcript.push_assistant(response.clone());
            if control.cancel.is_cancelled() {
                steps.push(record(step, response, vec![], vec![], notes));
                return Ok(self.result(&scope, AgentStatus::Cancelled, last_response, steps));
            }

            // ---- interpret the reply -----------------------------------------------------
            let parsed = parse_response(&response);
            if parsed.calls.is_empty() {
                if !parsed.malformed.is_empty() {
                    malformed_streak += 1;
                    notes.extend(parsed.malformed.iter().cloned());
                    for m in &parsed.malformed {
                        ev.emit(AgentEvent::Warning {
                            message: format!("step {step}: {m}"),
                        });
                    }
                    if malformed_streak > MAX_MALFORMED_STREAK {
                        steps.push(record(step, response, vec![], vec![], notes));
                        return Err(format!(
                            "model emitted malformed tool calls {malformed_streak} times in a row: {}",
                            parsed.malformed.join("; ")
                        ));
                    }
                    let fb = format!(
                        "[TOOL_ERROR]\n{}\nRe-send the call as valid JSON between [TOOL_CALL] and [/TOOL_CALL] markers, enclosing the JSON in a ```json code block:\n[TOOL_CALL]\n```json\n{{\"tool\": \"<name>\", \"args\": {{...}}}}\n```\n[/TOOL_CALL]",
                        parsed.malformed.join("\n")
                    );
                    transcript.push_user(fb.clone(), false);
                    outgoing.push(fb);
                    steps.push(record(step, response, vec![], vec![], notes));
                    continue;
                }

                // A user message that arrived while the model was thinking keeps the run going.
                let late = control.inbox.drain();
                if !late.is_empty() {
                    for msg in late {
                        self.backend.add_artifacts(&msg.artifacts)?;
                        let msg = msg.message;
                        ev.emit(AgentEvent::Steered {
                            message: msg.clone(),
                        });
                        notes.push(format!("user message: {msg}"));
                        let part = format!("[USER_MESSAGE]\n{msg}");
                        transcript.push_user(part.clone(), false);
                        outgoing.push(part);
                    }
                    steps.push(record(step, response, vec![], vec![], notes));
                    continue;
                }

                steps.push(record(step, response.clone(), vec![], vec![], notes));
                return Ok(self.result(&scope, AgentStatus::Completed, response, steps));
            }
            malformed_streak = 0;

            // ---- execute tools -----------------------------------------------------------
            let mut calls = parsed.calls;
            let mut truncated_calls = 0usize;
            if calls.len() > max_calls {
                truncated_calls = calls.len() - max_calls;
                calls.truncate(max_calls);
                notes.push(format!(
                    "{truncated_calls} tool call(s) beyond the per-step limit were not executed"
                ));
            }

            let plan = self.config.approval_policy.evaluate_batch(&calls);
            let mut user_approved = true;
            if plan.needs_user_approval() {
                let prompt_calls = plan.prompt_calls(&calls);
                let approval_id = crate::domain::id("approval");
                let _approval_guard = control.approvals.register_guard(&approval_id);
                ev.emit(AgentEvent::ToolApprovalRequested {
                    id: approval_id.clone(),
                    step,
                    calls: prompt_calls,
                    call_indices: plan.prompt_indices.clone(),
                    source: None,
                });
                match control.approvals.wait(&approval_id, &control.cancel) {
                    Some(approved) => {
                        user_approved = approved;
                        ev.emit(AgentEvent::ToolApprovalResolved {
                            id: approval_id,
                            step,
                            approved,
                        });
                        if !approved {
                            notes.push(format!(
                                "user rejected {} requested tool call(s) in step {step}",
                                plan.prompt_indices.len()
                            ));
                        }
                    }
                    None => {
                        steps.push(record(step, response, calls, vec![], notes));
                        return Ok(self.result(
                            &scope,
                            AgentStatus::Cancelled,
                            last_response,
                            steps,
                        ));
                    }
                }
            }

            let exec_indices: Vec<usize> = plan
                .decisions
                .iter()
                .enumerate()
                .filter_map(|(i, decision)| match decision {
                    PermissionDecision::Allow => Some(i),
                    PermissionDecision::Ask if user_approved => Some(i),
                    PermissionDecision::Ask | PermissionDecision::Deny { .. } => None,
                })
                .collect();

            let emit_started = |index: usize, call: &ToolCallRequest| {
                ev.emit(AgentEvent::ToolStarted {
                    step,
                    index,
                    tool: call.tool.clone(),
                    args: call.args.clone(),
                })
            };
            let emit_output =
                |index: usize, call: &ToolCallRequest, stream: OutputStream, text: &str| {
                    ev.emit(AgentEvent::ToolOutputChunk {
                        step,
                        index,
                        tool: call.tool.clone(),
                        stream,
                        text: text.to_owned(),
                    })
                };
            let emit_finished = |index: usize, call: &ToolCallRequest, out: &ToolOutput| {
                ev.emit(AgentEvent::ToolFinished {
                    step,
                    index,
                    tool: call.tool.clone(),
                    ok: out.ok,
                    output: out.output.clone(),
                    elapsed_ms: out.elapsed_ms,
                })
            };

            let observations = if exec_indices.len() == calls.len() {
                self.registry.execute_batch(
                    &calls,
                    scope.ctx,
                    &emit_started,
                    &emit_output,
                    &emit_finished,
                )
            } else {
                let exec_calls: Vec<ToolCallRequest> =
                    exec_indices.iter().map(|&i| calls[i].clone()).collect();
                let exec_outputs = if exec_calls.is_empty() {
                    Vec::new()
                } else {
                    self.registry.execute_batch(
                        &exec_calls,
                        scope.ctx,
                        &|local_idx, call| emit_started(exec_indices[local_idx], call),
                        &|local_idx, call, stream, text| {
                            emit_output(exec_indices[local_idx], call, stream, text)
                        },
                        &|local_idx, call, out| emit_finished(exec_indices[local_idx], call, out),
                    )
                };
                let mut exec_iter = exec_outputs.into_iter();
                calls
                    .iter()
                    .zip(plan.decisions.iter())
                    .map(|(call, decision)| match (decision, user_approved) {
                        (PermissionDecision::Allow, _) | (PermissionDecision::Ask, true) => {
                            exec_iter.next().unwrap_or_else(|| {
                                rejected_output(&call.tool, "Tool execution missing.")
                            })
                        }
                        (PermissionDecision::Ask, false) => {
                            rejected_output(&call.tool, "Tool execution rejected by user.")
                        }
                        (PermissionDecision::Deny { reason }, _) => {
                            let msg = if reason.trim().is_empty() {
                                "Tool execution denied by policy.".to_owned()
                            } else {
                                format!("Tool execution denied by policy: {}", reason.trim())
                            };
                            rejected_output(&call.tool, &msg)
                        }
                    })
                    .collect()
            };
            // Help must reach the model before an action can use it. A help call
            // cannot unlock browser actions from the same model response.
            if calls.iter().zip(&observations).any(|(call, out)| {
                call.tool == "browser_control" && call.args["action"] == "help" && out.ok
            }) {
                ctx.acknowledge_browser_help();
            }
            let artifacts = observations
                .iter()
                .filter(|out| out.ok)
                .flat_map(|out| out.artifacts.iter().cloned())
                .collect::<Vec<_>>();
            self.backend.set_step_artifacts(&artifacts)?;

            // ---- loop detection ----------------------------------------------------------
            let signature: Vec<String> = calls
                .iter()
                .map(|c| format!("{}:{}", c.tool, c.args))
                .collect();
            if signature == last_signature {
                repeat_count += 1;
            } else {
                repeat_count = 1;
                last_signature = signature;
            }
            let mut feedback = format_observations(&observations);
            if truncated_calls > 0 {
                feedback.push_str(&format!(
                    "\n\n[NOTE] Only the first {max_calls} tool calls were executed; {truncated_calls} more were skipped. Re-issue them if still needed."
                ));
            }
            if repeat_count >= REPEAT_WARN_THRESHOLD {
                let msg =
                    format!("the same tool call(s) were issued {repeat_count} times in a row");
                ev.emit(AgentEvent::Warning {
                    message: format!("step {step}: {msg}"),
                });
                notes.push(msg);
                feedback.push_str(
                    "\n\n[WARNING] You are repeating the same tool call(s) with the same arguments. Change your approach (re-read the error, inspect other files) or give your final answer.",
                );
            }
            feedback.push_str("\n\nNext tool call or final answer:");
            transcript.push_user(feedback.clone(), true);
            outgoing.push(feedback);

            steps.push(record(step, response, calls, observations, notes));
            if control.cancel.is_cancelled() {
                return Ok(self.result(&scope, AgentStatus::Cancelled, last_response, steps));
            }
        }

        ev.emit(AgentEvent::Warning {
            message: format!(
                "step budget of {max_steps} exhausted before the model gave a final answer"
            ),
        });
        Ok(self.result(&scope, AgentStatus::MaxSteps, last_response, steps))
    }
}

/// Immutable per-run state needed to build the final result.
struct RunScope<'a> {
    task: &'a str,
    start: Instant,
    control: &'a RunControl,
    ctx: &'a ToolContext,
}

fn record(
    step: usize,
    model_response: String,
    tool_calls: Vec<ToolCallRequest>,
    observations: Vec<ToolOutput>,
    notes: Vec<String>,
) -> AgentStepRecord {
    AgentStepRecord {
        step,
        model_response,
        tool_calls,
        observations,
        notes,
    }
}

fn format_observations(observations: &[ToolOutput]) -> String {
    let body: Vec<String> = observations
        .iter()
        .map(|o| {
            format!(
                "[tool: {} | status: {}]\n{}",
                o.tool,
                if o.ok { "ok" } else { "error" },
                o.output
            )
        })
        .collect();
    format!("[TOOL_RESULTS]\n{}", body.join("\n\n"))
}

fn rejected_output(tool: &str, message: &str) -> ToolOutput {
    ToolOutput {
        tool: tool.to_string(),
        ok: false,
        output: message.to_string(),
        artifacts: Vec::new(),
        elapsed_ms: 0,
    }
}
