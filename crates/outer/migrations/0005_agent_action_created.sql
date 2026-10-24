-- The agent action log's daily prune deletes by age across all apps.
CREATE INDEX idx_agent_action_created ON agent_action (created_at);
