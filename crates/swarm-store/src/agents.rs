//! Agent records and checkpoints.

use crate::Store;
use anyhow::{Context, Result};
use sqlx::Row;
use swarm_core::{AgentId, AgentRole, RunId, TaskId, agent::{Agent, AgentCheckpoint}, state::AgentState};

impl Store {
    pub async fn create_agent(&self, agent: &Agent) -> Result<()> {
        sqlx::query(
            "INSERT INTO agents (id, run_id, task_id, parent_id, role, state, depth, steps, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(agent.id.as_str())
        .bind(agent.run_id.as_str())
        .bind(agent.task_id.as_str())
        .bind(agent.parent_id.as_ref().map(|p| p.as_str()))
        .bind(agent.role.as_str())
        .bind(agent.state.as_str())
        .bind(agent.depth as i64)
        .bind(agent.steps as i64)
        .bind(agent.created_at)
        .bind(agent.updated_at)
        .execute(&self.pool)
        .await
        .context("insert agent")?;
        Ok(())
    }

    /// Persists the agent's mutable fields (state, step count, timestamp).
    /// Errors if no row matches the id.
    pub async fn update_agent(&self, agent: &Agent) -> Result<()> {
        let n = sqlx::query("UPDATE agents SET state = ?, steps = ?, updated_at = ? WHERE id = ?")
            .bind(agent.state.as_str())
            .bind(agent.steps as i64)
            .bind(agent.updated_at)
            .bind(agent.id.as_str())
            .execute(&self.pool)
            .await
            .context("update agent")?;
        if n.rows_affected() == 0 {
            anyhow::bail!("agent not found: {}", agent.id.as_str());
        }
        Ok(())
    }

    pub async fn load_agent(&self, id: &AgentId) -> Result<Option<Agent>> {
        let row = sqlx::query("SELECT * FROM agents WHERE id = ?")
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(row_to_agent).transpose()?)
    }

    pub async fn agents_for_run(&self, run: &RunId) -> Result<Vec<Agent>> {
        let rows = sqlx::query("SELECT * FROM agents WHERE run_id = ? ORDER BY created_at, id")
            .bind(run.as_str())
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(row_to_agent)
            .collect::<Result<Vec<_>, _>>()
    }

    /// Saves an agent's checkpoint (overwriting the previous one) and records
    /// the conversation size so compaction/restart can restore context.
    pub async fn save_checkpoint(
        &self,
        agent_id: &AgentId,
        run: &RunId,
        checkpoint: &AgentCheckpoint,
        now: i64,
    ) -> Result<()> {
        let cp_json = serde_json::to_string(checkpoint)?;
        let ctx_json = "[]".to_string();
        sqlx::query(
            "INSERT INTO agent_checkpoints (agent_id, run_id, step, steps_taken, conversation_len, checkpoint_json, context_json, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(agent_id) DO UPDATE SET step = excluded.step, steps_taken = excluded.steps_taken, \
             conversation_len = excluded.conversation_len, checkpoint_json = excluded.checkpoint_json, \
             context_json = excluded.context_json, created_at = excluded.created_at",
        )
        .bind(agent_id.as_str())
        .bind(run.as_str())
        .bind(checkpoint.steps)
        .bind(checkpoint.steps as i64)
        .bind(checkpoint.messages.len() as i64)
        .bind(cp_json)
        .bind(ctx_json)
        .bind(now)
        .execute(&self.pool)
        .await
        .context("save checkpoint")?;
        Ok(())
    }

    pub async fn load_checkpoint(
        &self,
        agent: &AgentId,
    ) -> Result<Option<AgentCheckpoint>> {
        let row = sqlx::query("SELECT checkpoint_json FROM agent_checkpoints WHERE agent_id = ?")
            .bind(agent.as_str())
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => {
                let s: String = row.try_get("checkpoint_json")?;
                Ok(Some(serde_json::from_str(&s)?))
            }
            None => Ok(None),
        }
    }
}

pub(crate) fn row_to_agent(row: &sqlx::sqlite::SqliteRow) -> Result<Agent> {
    let role_str: String = row.try_get("role")?;
    let state_str: String = row.try_get("state")?;
    Ok(Agent {
        id: AgentId::parse(&row.try_get::<String, _>("id")?)?,
        run_id: RunId::parse(&row.try_get::<String, _>("run_id")?)?,
        task_id: TaskId::parse(&row.try_get::<String, _>("task_id")?)?,
        parent_id: row
            .try_get::<Option<String>, _>("parent_id")?
            .map(|s| AgentId::parse(&s))
            .transpose()?,
        role: role_str.parse::<AgentRole>()?,
        state: state_str.parse::<AgentState>()?,
        depth: row.try_get::<i64, _>("depth")? as u32,
        steps: row.try_get::<i64, _>("steps")? as u32,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_util;
    use swarm_core::budget::{BudgetUsage, ResourceBudget};
    use swarm_core::run::Run;
    use swarm_core::task::Task;
    use swarm_core::TaskState;

    #[tokio::test]
    async fn agent_and_checkpoint_round_trip() {
        let (store, _tmp) = tests_util::open_test_store().await;
        let run = Run::new("t", "mock", None, 1, true, ResourceBudget::default(), 1);
        store.create_run(&run).await.unwrap();
        let run_id = run.id.clone();
        let task = Task {
            id: TaskId::new(),
            run_id: run_id.clone(),
            parent_id: None,
            depth: 0,
            title: "t".into(),
            objective: "o".into(),
            role: AgentRole::Worker,
            group: "g".into(),
            priority: 0,
            depends_on: vec![],
            state: TaskState::Ready,
            attempts: 0,
            created_at: 1,
            updated_at: 1,
        };
        store.create_task(&task).await.unwrap();
        let agent = Agent {
            id: AgentId::new(),
            run_id: run_id.clone(),
            task_id: task.id,
            parent_id: None,
            role: AgentRole::Worker,
            state: AgentState::Created,
            depth: 0,
            steps: 0,
            created_at: 1,
            updated_at: 1,
        };
        store.create_agent(&agent).await.unwrap();
        let loaded = store.load_agent(&agent.id).await.unwrap().unwrap();
        assert_eq!(loaded.role, AgentRole::Worker);

        let cp = AgentCheckpoint {
            version: AgentCheckpoint::VERSION,
            steps: 3,
            format_retries: 0,
            messages: vec![],
            tool_calls: vec![],
            child_task_ids: vec![],
            usage: BudgetUsage::default(),
        };
        store.save_checkpoint(&agent.id, &run_id, &cp, 100).await.unwrap();
        let cp = store.load_checkpoint(&agent.id).await.unwrap().unwrap();
        assert_eq!(cp.steps, 3);
    }

    #[tokio::test]
    async fn update_agent_persists_state_and_steps() {
        let (store, _tmp) = tests_util::open_test_store().await;
        let run = Run::new("t", "mock", None, 1, true, ResourceBudget::default(), 1);
        store.create_run(&run).await.unwrap();
        let run_id = run.id.clone();
        let task = Task {
            id: TaskId::new(),
            run_id: run_id.clone(),
            parent_id: None,
            depth: 0,
            title: "t".into(),
            objective: "o".into(),
            role: AgentRole::Worker,
            group: "g".into(),
            priority: 0,
            depends_on: vec![],
            state: TaskState::Ready,
            attempts: 0,
            created_at: 1,
            updated_at: 1,
        };
        store.create_task(&task).await.unwrap();
        let mut agent = Agent {
            id: AgentId::new(),
            run_id,
            task_id: task.id,
            parent_id: None,
            role: AgentRole::Worker,
            state: AgentState::Created,
            depth: 0,
            steps: 0,
            created_at: 1,
            updated_at: 1,
        };
        store.create_agent(&agent).await.unwrap();

        agent.state = AgentState::Running;
        agent.steps = 2;
        agent.updated_at = 200;
        store.update_agent(&agent).await.unwrap();
        let loaded = store.load_agent(&agent.id).await.unwrap().unwrap();
        assert_eq!(loaded.state, AgentState::Running);
        assert_eq!(loaded.steps, 2);
        assert_eq!(loaded.updated_at, 200);

        // Updating an id with no matching row is an error.
        let ghost = Agent {
            id: AgentId::new(),
            ..agent
        };
        assert!(store.update_agent(&ghost).await.is_err());
    }
}
