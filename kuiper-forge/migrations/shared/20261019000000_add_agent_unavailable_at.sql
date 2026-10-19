-- When an admin marked the agent unavailable (maintenance, image updates). NULL means it takes work.
ALTER TABLE registered_agents ADD COLUMN unavailable_at TEXT;
