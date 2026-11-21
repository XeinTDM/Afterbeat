use serde::{Deserialize, Serialize};

use crate::{
    mouse::Point,
    targets::{Action, ImageOutput, Step},
};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Operation {
    If { test: PixelTest },
    Else,
    EndIf,
    Repeat { count: u16 },
    EndRepeat,
    BreakIf { test: PixelTest },
    Recover { retries: u8, skip: bool },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct PixelTest {
    pub point: Point,
    pub rgb: u32,
    pub tolerance: u8,
    pub different: bool,
}

impl Operation {
    pub fn label(&self) -> String {
        let describe = |test: &PixelTest| {
            format!(
                "pixel {}, {} {} #{:06X} (±{})",
                test.point.x,
                test.point.y,
                if test.different {
                    "differs from"
                } else {
                    "matches"
                },
                test.rgb,
                test.tolerance,
            )
        };
        match self {
            Self::If { test } => format!("If {}", describe(test)),
            Self::Else => "Else".into(),
            Self::EndIf => "End if".into(),
            Self::Repeat { count } => format!("Repeat {count} times"),
            Self::EndRepeat => "End repeat".into(),
            Self::BreakIf { test } => format!("Exit repeat if {}", describe(test)),
            Self::Recover { retries, skip } => format!(
                "On next step failure · {retries} retries · {}",
                if *skip { "Skip" } else { "Stop" }
            ),
        }
    }

    pub fn validate(&self, has_target: bool, delay_ms: u32) -> Result<(), &'static str> {
        if has_target || delay_ms != 0 {
            return Err("Workflow steps do not take a target or delay.");
        }
        match self {
            Self::If { test } | Self::BreakIf { test }
                if test.rgb > 0xffffff || (test.different && test.tolerance == 255) =>
            {
                Err("Enter a valid pixel color and tolerance for the condition.")
            }
            Self::Repeat { count } if !(1..=1000).contains(count) => {
                Err("Repeat count must be 1–1,000.")
            }
            Self::Recover { retries, .. } if *retries > 10 => Err("Set retries from 0 to 10."),
            _ => Ok(()),
        }
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Copy)]
pub struct Recovery {
    pub retries: u8,
    pub skip: bool,
}

pub struct Plan {
    pub jumps: Vec<usize>,
    pub recovery: Vec<Option<Recovery>>,
}

enum Block {
    If {
        start: usize,
        otherwise: Option<usize>,
    },
    Repeat {
        start: usize,
        exits: Vec<usize>,
    },
}

fn action(step: &Step) -> Option<&Action> {
    match step {
        Step::Action(step) => Some(&step.action),
        _ => None,
    }
}

pub fn compile(route: &[Step]) -> Result<Plan, &'static str> {
    let mut plan = Plan {
        jumps: vec![0; route.len()],
        recovery: vec![None; route.len()],
    };
    let mut stack = Vec::new();
    for (index, step) in route.iter().enumerate() {
        let next = route.get(index + 1).and_then(action);
        match action(step) {
            Some(Action::WaitImage {
                output: ImageOutput::If,
                ..
            }) => {
                stack.push(Block::If {
                    start: index,
                    otherwise: None,
                });
            }
            Some(
                Action::WaitColor {
                    skip_next_on_timeout: true,
                    ..
                }
                | Action::WaitColor {
                    skip_next_on_mismatch: true,
                    ..
                },
            ) if matches!(
                next,
                Some(
                    Action::Flow { .. }
                        | Action::WaitImage {
                            output: ImageOutput::If,
                            ..
                        }
                )
            ) =>
            {
                return Err("Skip next cannot skip a workflow step.");
            }
            Some(Action::Flow { operation }) => match operation {
                Operation::If { .. } => stack.push(Block::If {
                    start: index,
                    otherwise: None,
                }),
                Operation::Else => match stack.last_mut() {
                    Some(Block::If { start, otherwise }) if otherwise.is_none() => {
                        *otherwise = Some(index);
                        plan.jumps[*start] = index + 1;
                    }
                    _ => return Err("Else needs an open If without an Else."),
                },
                Operation::EndIf => match stack.pop() {
                    Some(Block::If { start, otherwise }) => {
                        plan.jumps[otherwise.unwrap_or(start)] = index + 1
                    }
                    _ => return Err("End if must close the innermost If."),
                },
                Operation::Repeat { .. } => stack.push(Block::Repeat {
                    start: index,
                    exits: Vec::new(),
                }),
                Operation::EndRepeat => match stack.pop() {
                    Some(Block::Repeat { start, exits }) => {
                        plan.jumps[index] = start;
                        for exit in exits {
                            plan.jumps[exit] = index;
                        }
                    }
                    _ => return Err("End repeat must close the innermost Repeat."),
                },
                Operation::BreakIf { .. } => {
                    let exits = stack
                        .iter_mut()
                        .rev()
                        .find_map(|block| match block {
                            Block::Repeat { exits, .. } => Some(exits),
                            _ => None,
                        })
                        .ok_or("Exit repeat if must be inside a Repeat.")?;
                    exits.push(index);
                }
                Operation::Recover { retries, skip } => {
                    if !matches!(
                        next,
                        Some(
                            Action::Click { .. }
                                | Action::Scroll { .. }
                                | Action::Key { .. }
                                | Action::WaitImage {
                                    output: ImageOutput::Wait
                                        | ImageOutput::Click
                                        | ImageOutput::Move,
                                    ..
                                }
                                | Action::WaitColor {
                                    retry_clicks: 0,
                                    skip_next_on_timeout: false,
                                    skip_next_on_mismatch: false,
                                    ..
                                }
                        )
                    ) {
                        return Err(
                            "Recovery must precede a custom click, scroll, key, image, or pixel step without its own failure action.",
                        );
                    }
                    plan.recovery[index + 1] = Some(Recovery {
                        retries: *retries,
                        skip: *skip,
                    });
                }
            },
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err("Close every If and Repeat block before starting.");
    }
    Ok(plan)
}

pub fn sends_input(action: &Action) -> bool {
    !matches!(
        action,
        Action::Wait
            | Action::WaitColor { .. }
            | Action::Flow { .. }
            | Action::WaitImage {
                output: ImageOutput::Wait | ImageOutput::If,
                ..
            }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::ActionStep;

    fn flow(operation: Operation) -> Step {
        Step::Action(Box::new(ActionStep {
            action: Action::Flow { operation },
            target: None,
            delay_ms: 0,
        }))
    }

    #[test]
    fn nested_blocks_and_recovery_resolve_to_the_correct_steps() {
        let test = PixelTest {
            point: Point { x: 1, y: 1 },
            rgb: 0,
            tolerance: 0,
            different: false,
        };
        let route = [
            flow(Operation::Repeat { count: 3 }),
            flow(Operation::If { test: test.clone() }),
            flow(Operation::Repeat { count: 2 }),
            flow(Operation::BreakIf { test: test.clone() }),
            flow(Operation::EndRepeat),
            flow(Operation::Else),
            flow(Operation::Recover {
                retries: 2,
                skip: true,
            }),
            Step::Action(Box::new(ActionStep {
                action: Action::Key {
                    key: "Enter".into(),
                },
                target: None,
                delay_ms: 0,
            })),
            flow(Operation::EndIf),
            flow(Operation::EndRepeat),
        ];
        let plan = compile(&route).unwrap();
        assert_eq!(plan.jumps[1], 6);
        assert_eq!(plan.jumps[3], 4);
        assert_eq!(plan.jumps[4], 2);
        assert_eq!(plan.jumps[5], 9);
        assert_eq!(plan.jumps[9], 0);
        let recovery = plan.recovery[7].unwrap();
        assert_eq!(recovery.retries, 2);
        assert!(recovery.skip);
        assert!(compile(&route[..9]).is_err());
        assert!(compile(&[flow(Operation::BreakIf { test })]).is_err());
        assert!(compile(&[flow(Operation::Repeat { count: 2 }), flow(Operation::Else)]).is_err());
        assert!(
            compile(&[
                flow(Operation::Recover {
                    retries: 0,
                    skip: true
                }),
                flow(Operation::EndIf)
            ])
            .is_err()
        );
    }
}
