//! The interactive rebase planner: reorder commits and choose what happens
//! to each one, then run it all as one rebase.

use anyhow::{Result, bail};

use super::cmd::Purpose;
use super::{App, Level};
use crate::git::ObjectId;
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
                    Some(m) if !rebase::is_blank_message(m) => Action::Reword(m.clone()),
                    _ => bail!(
                        "Commit {} is set to reword but has no new message.",
                        r.id.short()
                    ),
                },
                PlanAction::Squash => match &r.message {
                    Some(m) if !rebase::is_blank_message(m) => Action::Squash(m.clone()),
                    _ => bail!(
                        "Commit {} is set to squash but has no message.",
                        r.id.short()
                    ),
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
                first.id.short(),
                first.action.name()
            );
        }
        // The commit before them also can't be a dropped one.
        for w in steps.windows(2) {
            if w[1].action.melds() && w[0].action == Action::Drop {
                bail!("A commit can't be squashed or fixed up into a dropped commit.");
            }
        }
        let plan = Plan {
            base: self.base,
            steps,
        };
        rebase::check(&plan)?;
        Ok(plan)
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
            self.set_status(
                "The oldest commit has nothing below it to meld into",
                Level::Warn,
            );
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
            self.open_planner_input(title, &m, id);
            return;
        }
        let below = p.rows.get(i + 1).map(|b| (b.id, b.message.clone()));
        self.spawn_read(
            move |r| async move {
                let this = ops::commit_message(&r.git, &id.to_string())
                    .await
                    .unwrap_or_default();
                match (action, below) {
                    (PlanAction::Squash, Some((below_id, below_msg))) => {
                        let below_msg = match below_msg {
                            Some(m) => m,
                            None => ops::commit_message(&r.git, &below_id.to_string())
                                .await
                                .unwrap_or_default(),
                        };
                        format!("{below_msg}\n\n{this}")
                    }
                    _ => this,
                }
            },
            move |app, initial| {
                // The rows may have moved in the meantime, so find this one
                // by its commit. Only ask if it still wants a message.
                let Some(row) = app
                    .planner
                    .as_mut()
                    .and_then(|p| p.rows.iter_mut().find(|r| r.id == id))
                else {
                    return;
                };
                if row.action != action || row.message.is_some() {
                    return;
                }
                if app.overlays.is_empty() {
                    app.open_planner_input(title, &initial, id);
                } else {
                    // Another dialog opened, so the user has moved on. As
                    // when the dialog is cancelled, a row can't keep reword
                    // or squash without a message.
                    row.action = PlanAction::Pick;
                }
            },
        );
    }

    fn open_planner_input(&mut self, title: &str, initial: &str, id: ObjectId) {
        self.overlays
            .push(super::cmd::Overlay::Input(super::cmd::InputBox {
                title: title.into(),
                hint: "Ctrl+Enter to save, Esc to cancel".into(),
                text: crate::ui::textarea::TextArea::with_text(true, initial),
                purpose: Purpose::PlannerMessage(id),
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
                rebase::Outcome::Stopped => {
                    "Rebase stopped. Make your changes, then continue (,C).".to_string()
                }
            })
        });
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

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

    #[test]
    fn refuses_blank_messages_and_dropping_everything() {
        let mut p = planner(3);
        p.rows[1].action = PlanAction::Reword;
        p.rows[1].message = Some(" \n\t\n".into());
        assert!(p.to_plan().is_err());
        p.rows[1].message = Some("\n new\n".into());
        assert!(p.to_plan().is_ok());

        // The plan starts at the root, so dropping every commit would leave
        // the branch with none.
        for r in &mut p.rows {
            r.action = PlanAction::Drop;
        }
        assert!(p.to_plan().is_err());
        p.base = Some(ObjectId::null());
        assert!(p.to_plan().is_ok());
    }

    fn action() -> impl Strategy<Value = PlanAction> {
        prop::sample::select(PlanAction::ALL.to_vec())
    }

    fn message() -> impl Strategy<Value = Option<String>> {
        prop::option::of(prop::sample::select(vec![
            "",
            " \n",
            "new",
            "#1 new\n\nbody",
        ]))
        .prop_map(|m| m.map(str::to_string))
    }

    proptest! {
        /// Moves only reorder the rows, and leave the selection on one.
        /// Every plan that `to_plan` accepts can be written as a todo list,
        /// with one line for each step in order, and an `exec` line after
        /// each reword and squash.
        #[test]
        fn moves_and_todo_lists(
            n in 1u8..8,
            root in any::<bool>(),
            moves in prop::collection::vec((any::<bool>(), 0usize..8, 0usize..8, any::<bool>()), 0..16),
            actions in prop::collection::vec((action(), message()), 8),
        ) {
            let mut p = planner(n);
            if !root {
                p.base = Some(ObjectId::null());
            }
            let n = usize::from(n);
            for (by_row, a, b, up) in moves {
                if by_row {
                    p.move_row(a, b);
                } else {
                    p.selected = a % n;
                    p.move_selected(up);
                }
                prop_assert!(p.selected < n);
            }
            let mut ids: Vec<ObjectId> = p.rows.iter().map(|r| r.id).collect();
            ids.sort();
            let mut want: Vec<ObjectId> = planner(n as u8).rows.iter().map(|r| r.id).collect();
            want.sort();
            prop_assert_eq!(ids, want);

            for (r, (action, message)) in p.rows.iter_mut().zip(actions) {
                r.action = action;
                r.message = message;
            }
            let Ok(plan) = p.to_plan() else {
                return Ok(());
            };
            let dir = tempfile::tempdir().expect("tempdir");
            let todo = rebase::build_todo(&plan, dir.path());
            prop_assert!(todo.is_ok(), "{:?}", todo);
            let todo = String::from_utf8_lossy(&todo.expect("todo")).into_owned();
            let mut lines = todo.lines();
            for (step, row) in plan.steps.iter().zip(p.rows.iter().rev()) {
                prop_assert_eq!(step.id, row.id);
                let line = lines.next().unwrap_or_default();
                let verb = match step.action {
                    Action::Pick | Action::Reword(_) => "pick",
                    Action::Fixup | Action::Squash(_) => "fixup",
                    Action::Drop => "drop",
                    Action::Edit => "edit",
                };
                prop_assert!(
                    line.starts_with(&format!("{verb} {} ", step.id)),
                    "{} for {:?}", line, step
                );
                if matches!(step.action, Action::Reword(_) | Action::Squash(_)) {
                    let exec = lines.next().unwrap_or_default();
                    prop_assert!(exec.starts_with("exec git commit --amend"), "{}", exec);
                }
            }
            prop_assert_eq!(lines.next(), None);
        }
    }
}
