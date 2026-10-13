//! Parse the structured plan in a fenced `swarm` reply block.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use swarm_core::protocol::{AgentPlan, PlanStep};

/// Returns `None` when a reply contains no `swarm` block. A malformed block
/// is an error so the planner can ask the model to repair its format.
pub fn parse_plan(reply: &str) -> Result<Option<AgentPlan>> {
    let Some(start) = reply.find("```swarm") else {
        return Ok(None);
    };
    let body_start = reply[start + "```swarm".len()..]
        .find('\n')
        .map(|n| start + "```swarm".len() + n + 1)
        .ok_or_else(|| anyhow!("swarm block has no JSON body"))?;
    let end = reply[body_start..]
        .find("```")
        .map(|n| body_start + n)
        .ok_or_else(|| anyhow!("swarm block is not closed"))?;
    let value: Value = serde_json::from_str(reply[body_start..end].trim())
        .context("swarm block is not valid JSON")?;
    let plan = value.get("plan").unwrap_or(&value);
    let steps = plan.get("steps").and_then(Value::as_array)
        .ok_or_else(|| anyhow!("plan.steps must be an array"))?;
    if steps.is_empty() {
        bail!("plan.steps must not be empty");
    }
    let mut parsed = Vec::with_capacity(steps.len());
    for (index, step) in steps.iter().enumerate() {
        let description = step.get("description").and_then(Value::as_str)
            .map(str::trim).filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("plan.steps[{index}].description is required"))?;
        if description.len() > 2_000 {
            bail!("plan.steps[{index}].description is too long");
        }
        let priority = step.get("priority").and_then(Value::as_i64).unwrap_or(0);
        let priority = i32::try_from(priority)
            .with_context(|| format!("plan.steps[{index}].priority is out of range"))?;
        parsed.push(PlanStep {
            description: description.to_string(),
            kind: step.get("kind").and_then(Value::as_str).map(str::to_string),
            priority,
        });
    }
    let string_array = |key: &str| -> Result<Vec<String>> {
        match plan.get(key) {
            None => Ok(Vec::new()),
            Some(Value::Array(values)) => values.iter().map(|v| {
                v.as_str().map(str::to_string)
                    .ok_or_else(|| anyhow!("plan.{key} must contain strings"))
            }).collect(),
            Some(_) => bail!("plan.{key} must be an array"),
        }
    };
    Ok(Some(AgentPlan {
        steps: parsed,
        expected_artifacts: string_array("expected_artifacts")?,
        success_criteria: string_array("success_criteria")?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plan_and_rejects_bad_blocks() {
        let plan = parse_plan("Here is the plan:\n```swarm\n{\"plan\":{\"steps\":[{\"description\":\"Build one part\"}]}}\n```\n")
            .unwrap().unwrap();
        assert_eq!(plan.steps[0].description, "Build one part");
        assert_eq!(plan.steps[0].priority, 0);
        assert!(parse_plan("No plan").unwrap().is_none());
        assert!(parse_plan("```swarm\n{broken}\n```").is_err());
        assert!(parse_plan("```swarm\n{\"steps\":[]}\n```").is_err());
    }
}
