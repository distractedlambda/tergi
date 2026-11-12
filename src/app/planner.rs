//! The interactive rebase planner: reorder commits and choose what happens
//! to each one, then run it all as one rebase.

use anyhow::{Result, bail};
use gix::ObjectId;

use super::cmd::Purpose;
use super::{App, Level};
use crate::git::ops;
use crate::git::rebase::{self, Action, Plan, Step};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanAction {
    Pick,
    Reword,
    Squash,
    Fixup,
    Drop,
    Edit,
}

impl PlanAction {
    pub const ALL: [PlanAction; 6] = [
        PlanAction::Pick,
        PlanAction::Reword,
        PlanAction::Squash,
        PlanAction::Fixup,
        PlanAction::Drop,
        PlanAction::Edit,
    ];

    pub fn name(self) -> &'static str {
        match self {
            PlanAction::Pick => "pick",
            PlanAction::Reword => "reword",
            PlanAction::Squash => "squash",
            PlanAction::Fixup => "fixup",
            PlanAction::Drop => "drop",
            PlanAction::Edit => "edit",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlanRow {
    pub id: ObjectId,
    pub summary: String,
    pub action: PlanAction,
    /// The new message for reword and squash.
    pub message: Option<String>,
}

/// Rows are shown newest first, like the history.
#[derive(Debug)]
pub struct Planner {
    pub base: Option<ObjectId>,
    pub rows: Vec<PlanRow>,
    pub selected: usize,
    pub scroll: usize,
}

impl Planner {
    pub fn new(plan: Plan) -> Planner {
        let rows = plan
            .steps
            .into_iter()
            .rev()
            .map(|s| PlanRow {
                id: s.id,
                summary: s.summary,
                action: PlanAction::Pick,
                message: None,
            })
            .collect();
        Planner {
            base: plan.base,
            rows,
            selected: 0,
            scroll: 0,
        }
    }

    pub fn move_selected(&mut self, up: bool) {
        let i = self.selected;
        let j = if up { i.wrapping_sub(1) } else { i + 1 };
        if j < self.rows.len() {
            self.rows.swap(i, j);
            self.selected = j;
        }
    }

    pub fn move_row(&mut self, from: usize, to: usize) {
        if from < self.rows.len() && to < self.rows.len() && from != to {
            let r = self.rows.remove(from);
            self.rows.insert(to, r);
            self.selected = to;
        }
    }

    pub fn to_plan(&self) -> Result<Plan> {
        let mut steps = Vec::new();
        for r in self.rows.iter().rev() {
            let action = match r.action {
                PlanAction::Pick => Action::Pick,
                PlanAction::Drop => Action::Drop,
                PlanAction::Edit => Action::Edit,
                PlanAction::Fixup => Action::Fixup,
                PlanAction::Reword => match &r.message {
                    Some(m) => Action::Reword(m.clone()),
                    None => bail!("Commit {} is set to reword but has no new message.", r.id.to_hex_with_len(7)),
                },
                PlanAction::Squash => match &r.message {
                    Some(m) => Action::Squash(m.clone()),
                    None => bail!("Commit {} is set to squash but has no message.", r.id.to_hex_with_len(7)),
                },
            };
            steps.push(Step {
                id: r.id,
                summary: r.summary.clone(),
                action,
            });
        }
        // Squash and fixup meld into the commit before them. Reordering can
        // leave such a commit at the bottom, with nothing before it.
        if let Some(first) = steps.first()
            && first.action.melds()
        {
            bail!(
                "Commit {} is the oldest one, so it has nothing below it to {} into.",
                first.id.to_hex_with_len(7),
                first.action.name()
            );
        }
        // The commit before them also can't be a dropped one.
        for w in steps.windows(2) {
            if w[1].action.melds() && w[0].action == Action::Drop {
                bail!("A commit can't be squashed or fixed up into a dropped commit.");
            }
        }
        Ok(Plan {
            base: self.base,
            steps,
        })
    }
}

impl App {
    pub fn planner_set(&mut self, action: PlanAction) {
        let Some(p) = &mut self.planner else { return };
        let i = p.selected;
        let n = p.rows.len();
        if n == 0 {
            return;
        }
        let oldest = i + 1 == n;
        if matches!(action, PlanAction::Squash | PlanAction::Fixup) && oldest {
            self.set_status("The oldest commit has nothing below it to meld into", Level::Warn);
            return;
        }
        if p.rows[i].action != action {
            // A message written for a reword doesn't fit a squash, and the
            // other way round.
            p.rows[i].message = None;
        }
        p.rows[i].action = action;
        let title = match action {
            PlanAction::Reword => "Reword commit",
            PlanAction::Squash => "Message for the squashed commit",
            _ => return,
        };
        let id = p.rows[i].id;
        if let Some(m) = p.rows[i].message.clone() {
            self.open_planner_input(title, &m, i);
            return;
        }
        let below = p.rows.get(i + 1).map(|b| (b.id, b.message.clone()));
        self.query(
            move |r| async move {
                let this = ops::commit_message(&r.git, &id.to_string()).await.unwrap_or_default();
                match (action, below) {
                    (PlanAction::Squash, Some((below_id, below_msg))) => {
                        let below_msg = match below_msg {
                            Some(m) => m,
                            None => ops::commit_message(&r.git, &below_id.to_string()).await.unwrap_or_default(),
                        };
                        format!("{below_msg}\n\n{this}")
                    }
                    _ => this,
                }
            },
            move |app, initial| {
                // The user may have changed the plan while we loaded.
                let unchanged = app
                    .planner
                    .as_ref()
                    .and_then(|p| p.rows.get(i))
                    .is_some_and(|r| r.id == id && r.action == action && r.message.is_none());
                if unchanged {
                    app.open_planner_input(title, &initial, i);
                }
            },
        );
    }

    fn open_planner_input(&mut self, title: &str, initial: &str, row: usize) {
        self.overlays.push(super::cmd::Overlay::Input(super::cmd::InputBox {
            title: title.into(),
            hint: "Ctrl+Enter to save, Esc to cancel".into(),
            text: crate::ui::textarea::TextArea::with_text(true, initial),
            purpose: Purpose::PlannerMessage(row),
        }));
    }

    pub fn planner_start(&mut self) {
        let Some(p) = &self.planner else { return };
        let plan = match p.to_plan() {
            Ok(plan) => plan,
            Err(e) => {
                self.show_error("Can't start the rebase", &e);
                return;
            }
        };
        let exe = self.repo.exe.clone();
        self.planner = None;
        self.spawn_op("Rebase", super::After::Nothing, move |r, _| async move {
            Ok(match rebase::run(&r.git, &plan, &exe).await? {
                rebase::Outcome::Done => "Rebase finished".to_string(),
                rebase::Outcome::Stopped => "Rebase stopped. Make your changes, then Continue (alt+c).".to_string(),
            })
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planner(n: u8) -> Planner {
        let steps = (1..=n)
            .map(|i| {
                let mut id = [0u8; 20];
                id[19] = i;
                Step {
                    id: ObjectId::from_bytes_or_panic(&id),
                    summary: format!("commit {i}"),
                    action: Action::Pick,
                }
            })
            .collect();
        Planner::new(Plan { base: None, steps })
    }

    #[test]
    fn rows_are_newest_first() {
        let mut p = planner(3);
        assert_eq!(p.rows[0].summary, "commit 3");
        p.rows[0].action = PlanAction::Fixup;
        let plan = p.to_plan().unwrap();
        assert_eq!(plan.steps[2].action, Action::Fixup);
        assert_eq!(plan.steps[0].summary, "commit 1");
    }

    #[test]
    fn refuses_to_meld_the_oldest_commit() {
        let mut p = planner(3);
        p.rows[1].action = PlanAction::Fixup;
        assert!(p.to_plan().is_ok());
        // Move the fixup to the bottom.
        p.selected = 1;
        p.move_selected(false);
        assert!(p.to_plan().unwrap_err().to_string().contains("oldest"));
    }
}
