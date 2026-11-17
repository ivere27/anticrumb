/// Conversation history kept by the agent loop.
///
/// With a persistent Gemini chat only the newest turn has to be sent, but temporary chats start
/// from scratch every step, so the whole transcript is re-rendered. To keep that bounded, old tool
/// observations are elided first (oldest first) and, as a last resort, whole old turns dropped.
#[derive(Debug, Clone)]
pub struct Transcript {
    turns: Vec<Turn>,
}

#[derive(Debug, Clone)]
struct Turn {
    role: Role,
    text: String,
    /// Observation turns can be replaced by a short stub when the budget is exceeded.
    elidable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    User,
    Assistant,
}

/// Number of newest turns that are never touched by compaction.
const KEEP_RECENT: usize = 2;

impl Transcript {
    pub fn new(initial_prompt: String) -> Self {
        Self {
            turns: vec![Turn {
                role: Role::User,
                text: initial_prompt,
                elidable: false,
            }],
        }
    }

    pub fn push_assistant(&mut self, text: impl Into<String>) {
        self.turns.push(Turn {
            role: Role::Assistant,
            text: text.into(),
            elidable: false,
        });
    }

    pub fn push_user(&mut self, text: impl Into<String>, elidable: bool) {
        self.turns.push(Turn {
            role: Role::User,
            text: text.into(),
            elidable,
        });
    }

    fn total_chars(turns: &[Turn]) -> usize {
        turns.iter().map(|t| t.text.chars().count() + 16).sum()
    }

    /// Render the transcript as one prompt, compacting it to roughly `budget_chars`.
    pub fn render(&self, budget_chars: usize) -> String {
        let mut turns = self.turns.clone();
        let n = turns.len();
        let protected_from = n.saturating_sub(KEEP_RECENT);

        // 1) elide old observations
        for i in 1..protected_from {
            if Self::total_chars(&turns) <= budget_chars {
                break;
            }
            if turns[i].elidable {
                let len = turns[i].text.chars().count();
                turns[i].text = format!("[older tool results elided: {len} chars]");
            }
        }

        // 2) drop whole old turns (keep the initial prompt + recent turns)
        let mut dropped = 0usize;
        while Self::total_chars(&turns) > budget_chars && turns.len() > KEEP_RECENT + 1 {
            let end = turns.len().saturating_sub(KEEP_RECENT);
            let Some(index) =
                (1..end).find(|i| turns[*i].role == Role::Assistant || turns[*i].elidable)
            else {
                break;
            };
            turns.remove(index);
            dropped += 1;
        }

        // Bound even the newest bulky observation. Keep all task/steering instructions.
        for i in 1..turns.len() {
            let over = Self::total_chars(&turns).saturating_sub(budget_chars.saturating_sub(256));
            if over == 0 {
                break;
            }
            if turns[i].elidable && turns[i].text.chars().count() > 256 {
                let keep = turns[i]
                    .text
                    .chars()
                    .count()
                    .saturating_sub(over + 128)
                    .max(128);
                let head: String = turns[i].text.chars().take(keep).collect();
                turns[i].text=format!("{head}\n[tool output excerpt truncated to fit context; read smaller ranges for missing detail]");
            }
        }
        let mut out = String::new();
        for (i, t) in turns.iter().enumerate() {
            if i == 0 {
                out.push_str(&t.text);
                continue;
            }
            if i == 1 && dropped > 0 {
                out.push_str(&format!(
                    "\n\n[{dropped} earlier turn(s) omitted to save context]"
                ));
            }
            let label = match t.role {
                Role::User => "User",
                Role::Assistant => "Assistant",
            };
            out.push_str(&format!("\n\n{label}:\n{}", t.text));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_in_full_when_under_budget() {
        let mut t = Transcript::new("TASK".into());
        t.push_assistant("a1");
        t.push_user("obs1", true);
        let r = t.render(10_000);
        assert_eq!(r, "TASK\n\nAssistant:\na1\n\nUser:\nobs1");
    }

    #[test]
    fn elides_old_observations_before_dropping_turns() {
        let mut t = Transcript::new("TASK".into());
        for i in 0..6 {
            t.push_assistant(format!("call {i}"));
            t.push_user(format!("{}{i}", "x".repeat(2_000)), true);
        }
        let r = t.render(5_000);
        assert!(r.starts_with("TASK"));
        assert!(r.contains("older tool results elided"));
        // newest observation stays intact
        assert!(r.contains(&format!("{}5", "x".repeat(2_000))));
        assert!(r.chars().count() < 6_000);
    }

    #[test]
    fn drops_turns_as_last_resort_but_keeps_task_and_tail() {
        let mut t = Transcript::new("TASK".into());
        for i in 0..10 {
            t.push_assistant(format!("{}{i}", "y".repeat(1_000)));
            t.push_user("ok", false);
        }
        let r = t.render(3_000);
        assert!(r.starts_with("TASK"));
        assert!(r.contains("earlier turn(s) omitted"));
        assert!(r.ends_with("ok"));
    }
}
