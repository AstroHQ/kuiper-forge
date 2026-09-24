-- last version the agent reported in its status. NULL for agents that predate version reporting
ALTER TABLE registered_agents ADD COLUMN agent_version TEXT;
