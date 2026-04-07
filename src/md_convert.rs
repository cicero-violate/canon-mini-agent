use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::constants::{INVARIANTS_FILE, INVARIANTS_MD_FILE, OBJECTIVES_FILE, OBJECTIVES_MD_FILE};
use crate::reports::{
    Invariant, InvariantCategory, InvariantLevel, InvariantsReport, Objective, ObjectiveCategory,
    ObjectiveLevel, ObjectivesReport,
};

pub fn ensure_objectives_and_invariants_json(workspace: &Path) -> Result<()> {
    let invariants_md = workspace.join(INVARIANTS_MD_FILE);
    let objectives_md = workspace.join(OBJECTIVES_MD_FILE);
    let invariants_json = workspace.join(INVARIANTS_FILE);
    let objectives_json = workspace.join(OBJECTIVES_FILE);

    if invariants_md.exists() {
        let text = std::fs::read_to_string(&invariants_md).unwrap_or_default();
        let report = parse_invariants_md(&text);
        write_json(&invariants_json, &report)?;
    }

    if objectives_md.exists() {
        let text = std::fs::read_to_string(&objectives_md).unwrap_or_default();
        let report = parse_objectives_md(&text);
        write_json(&objectives_json, &report)?;
    }

    Ok(())
}

fn write_json(path: &PathBuf, value: &impl serde::Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json = serde_json::to_string_pretty(value)?;
    std::fs::write(path, json)?;
    Ok(())
}

fn parse_invariants_md(text: &str) -> InvariantsReport {
    let mut invariants: Vec<Invariant> = Vec::new();
    let mut principles: Vec<String> = Vec::new();
    let mut math: Vec<String> = Vec::new();
    let mut meta: Vec<String> = Vec::new();

    let mut current_title: Option<String> = None;
    let mut current_clauses: Vec<String> = Vec::new();
    let mut current_desc: Vec<String> = Vec::new();
    let mut in_principles = false;
    let mut in_math = false;
    let mut in_meta = false;

    let push_current = |title_opt: &mut Option<String>,
                            clauses: &mut Vec<String>,
                            desc: &mut Vec<String>,
                            out: &mut Vec<Invariant>| {
        if let Some(title) = title_opt.take() {
            let description = desc.join(" ").trim().to_string();
            let invariant = Invariant {
                id: slugify(&title),
                title,
                category: categorize_invariant(&description),
                level: InvariantLevel::Medium,
                description,
                clauses: std::mem::take(clauses),
            };
            out.push(invariant);
            desc.clear();
        }
    };

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("## ") {
            let title = line.trim_start_matches("## ").trim();
            if title.eq_ignore_ascii_case("math") {
                push_current(&mut current_title, &mut current_clauses, &mut current_desc, &mut invariants);
                in_math = true;
                in_principles = false;
                in_meta = false;
                continue;
            }
            if title.starts_with("Additional Exhaustive") {
                push_current(&mut current_title, &mut current_clauses, &mut current_desc, &mut invariants);
                in_math = false;
                in_principles = false;
                in_meta = false;
                continue;
            }
            if title.starts_with("Meta-Level") || title.starts_with("Insight") || title.starts_with("Final") {
                push_current(&mut current_title, &mut current_clauses, &mut current_desc, &mut invariants);
                in_meta = true;
                in_math = false;
                in_principles = false;
                if !title.is_empty() {
                    meta.push(title.to_string());
                }
                continue;
            }
        }
        if line.starts_with("### ") {
            push_current(&mut current_title, &mut current_clauses, &mut current_desc, &mut invariants);
            current_title = Some(line.trim_start_matches("### ").trim().to_string());
            in_principles = false;
            in_math = false;
            in_meta = false;
            continue;
        }
        if line.ends_with("invariant") && !line.starts_with("-") && !line.starts_with("*") {
            let title = line.to_string();
            invariants.push(Invariant {
                id: slugify(&title),
                title,
                category: InvariantCategory::Other,
                level: InvariantLevel::High,
                description: String::new(),
                clauses: Vec::new(),
            });
            continue;
        }
        if line.starts_with("A closed-loop invariant system means") {
            in_principles = true;
            in_math = false;
            in_meta = false;
            continue;
        }
        if in_math {
            math.push(strip_bullet(line));
            continue;
        }
        if in_meta {
            meta.push(strip_bullet(line));
            continue;
        }
        if in_principles {
            principles.push(strip_bullet(line));
            continue;
        }
        if line.starts_with("- ") || line.starts_with("* ") {
            current_clauses.push(strip_bullet(line));
        } else if let Some(_) = current_title {
            current_desc.push(line.to_string());
        }
    }
    push_current(&mut current_title, &mut current_clauses, &mut current_desc, &mut invariants);

    InvariantsReport {
        version: 1,
        invariants,
        principles,
        math,
        meta,
    }
}

fn parse_objectives_md(text: &str) -> ObjectivesReport {
    let mut objectives: Vec<Objective> = Vec::new();
    let mut goal: Vec<String> = Vec::new();
    let mut instrumentation: Vec<String> = Vec::new();
    let mut definition_of_done: Vec<String> = Vec::new();
    let mut non_goals: Vec<String> = Vec::new();

    let mut current: Option<ObjectiveBuilder> = None;
    let mut section = ObjectiveSection::None;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("## ") {
            let title = line.trim_start_matches("## ").trim();
            if title.contains("Goal") {
                section = ObjectiveSection::Goal;
                if let Some(obj) = current.take() {
                    objectives.push(obj.finish());
                }
                continue;
            }
            if title.contains("Instrumentation") {
                section = ObjectiveSection::Instrumentation;
                if let Some(obj) = current.take() {
                    objectives.push(obj.finish());
                }
                continue;
            }
            if title.contains("Definition of Done") {
                section = ObjectiveSection::DefinitionDone;
                if let Some(obj) = current.take() {
                    objectives.push(obj.finish());
                }
                continue;
            }
            if title.contains("Non-Goals") {
                section = ObjectiveSection::NonGoals;
                if let Some(obj) = current.take() {
                    objectives.push(obj.finish());
                }
                continue;
            }
            if title.contains("Objective") || title.starts_with("OBJ-") {
                if let Some(obj) = current.take() {
                    objectives.push(obj.finish());
                }
                current = Some(ObjectiveBuilder::new(title));
                section = ObjectiveSection::None;
                continue;
            }
        }
        if line.starts_with("### ") {
            let title = line.trim_start_matches("### ").trim().to_lowercase();
            section = match title.as_str() {
                "requirement" => ObjectiveSection::Requirement,
                "verification" => ObjectiveSection::Verification,
                "success criteria" => ObjectiveSection::SuccessCriteria,
                _ => ObjectiveSection::None,
            };
            continue;
        }
        match section {
            ObjectiveSection::Goal => goal.push(strip_bullet(line)),
            ObjectiveSection::Instrumentation => instrumentation.push(strip_bullet(line)),
            ObjectiveSection::DefinitionDone => definition_of_done.push(strip_bullet(line)),
            ObjectiveSection::NonGoals => non_goals.push(strip_bullet(line)),
            ObjectiveSection::Requirement => {
                if let Some(obj) = current.as_mut() {
                    obj.requirement.push(strip_bullet(line));
                }
            }
            ObjectiveSection::Verification => {
                if let Some(obj) = current.as_mut() {
                    obj.verification.push(strip_bullet(line));
                }
            }
            ObjectiveSection::SuccessCriteria => {
                if let Some(obj) = current.as_mut() {
                    obj.success_criteria.push(strip_bullet(line));
                }
            }
            ObjectiveSection::None => {
                if let Some(obj) = current.as_mut() {
                    obj.description.push(line.to_string());
                }
            }
        }
    }
    if let Some(obj) = current.take() {
        objectives.push(obj.finish());
    }

    ObjectivesReport {
        version: 1,
        objectives,
        goal,
        instrumentation,
        definition_of_done,
        non_goals,
    }
}

struct ObjectiveBuilder {
    title: String,
    category: ObjectiveCategory,
    level: ObjectiveLevel,
    description: Vec<String>,
    requirement: Vec<String>,
    verification: Vec<String>,
    success_criteria: Vec<String>,
}

impl ObjectiveBuilder {
    fn new(title: &str) -> Self {
        let level = parse_objective_level(title);
        let category = categorize_objective(title);
        Self {
            title: title.to_string(),
            category,
            level,
            description: Vec::new(),
            requirement: Vec::new(),
            verification: Vec::new(),
            success_criteria: Vec::new(),
        }
    }

    fn finish(self) -> Objective {
        Objective {
            id: slugify(&self.title),
            title: self.title,
            category: self.category,
            level: self.level,
            description: self.description.join(" ").trim().to_string(),
            requirement: self.requirement,
            verification: self.verification,
            success_criteria: self.success_criteria,
        }
    }
}

#[derive(Clone, Copy)]
enum ObjectiveSection {
    None,
    Requirement,
    Verification,
    SuccessCriteria,
    Goal,
    Instrumentation,
    DefinitionDone,
    NonGoals,
}

fn parse_objective_level(title: &str) -> ObjectiveLevel {
    if title.contains("🔴") || title.to_lowercase().contains("critical") {
        ObjectiveLevel::Critical
    } else if title.contains("🟠") || title.to_lowercase().contains("high") {
        ObjectiveLevel::High
    } else if title.contains("🟡") || title.to_lowercase().contains("medium") {
        ObjectiveLevel::Medium
    } else {
        ObjectiveLevel::Low
    }
}

fn categorize_objective(title: &str) -> ObjectiveCategory {
    let t = title.to_lowercase();
    if t.contains("eventbus") {
        ObjectiveCategory::EventBusIntegrity
    } else if t.contains("hook") {
        ObjectiveCategory::HookSafety
    } else if t.contains("control flow") || t.contains("control-flow") || t.contains("cycle") {
        ObjectiveCategory::ControlFlowGuarantee
    } else if t.contains("deterministic") || t.contains("decision") {
        ObjectiveCategory::DecisionDeterminism
    } else if t.contains("async") {
        ObjectiveCategory::AsyncPropagation
    } else if t.contains("routing") {
        ObjectiveCategory::NoHiddenRouting
    } else if t.contains("instrumentation") {
        ObjectiveCategory::Instrumentation
    } else {
        ObjectiveCategory::Other
    }
}

fn categorize_invariant(text: &str) -> InvariantCategory {
    let t = text.to_lowercase();
    if t.contains("route") || t.contains("routing") {
        InvariantCategory::RoutingAuthority
    } else if t.contains("event") || t.contains("log") || t.contains("tick") {
        InvariantCategory::EventLogIntegrity
    } else if t.contains("policy") || t.contains("invariant") {
        InvariantCategory::PolicyGating
    } else if t.contains("deterministic") {
        InvariantCategory::Determinism
    } else if t.contains("plan") {
        InvariantCategory::Planning
    } else if t.contains("loop") || t.contains("cycle") {
        InvariantCategory::ControlLoop
    } else if t.contains("safety") || t.contains("idempotency") {
        InvariantCategory::Safety
    } else {
        InvariantCategory::Other
    }
}

fn strip_bullet(line: &str) -> String {
    let trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix("- ") {
        rest.trim().to_string()
    } else if let Some(rest) = trimmed.strip_prefix("* ") {
        rest.trim().to_string()
    } else {
        trimmed.to_string()
    }
}

fn slugify(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            if !out.ends_with('_') {
                out.push('_');
            }
        }
    }
    out.trim_matches('_').to_string()
}

#[allow(dead_code)]
fn json_preview(value: &impl serde::Serialize) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string())
}
