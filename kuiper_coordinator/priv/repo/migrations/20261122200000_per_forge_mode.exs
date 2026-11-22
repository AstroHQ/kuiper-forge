defmodule KuiperCoordinator.Repo.Migrations.PerForgeMode do
  use Ecto.Migration

  # the provisioning mode (and what only one mode needs: the fixed-capacity scope, the poll
  # interval) moves from the one settings row to each forge. the global runner group goes: fixed
  # capacity never used it, here or in the Rust coordinator
  def up do
    alter table(:forges) do
      add :mode, :string, null: false, default: "polling"
      add :runner_scope, :string
      add :poll_interval_s, :integer, null: false, default: 120
    end

    flush()

    execute """
    UPDATE forges SET poll_interval_s = s.poll_interval_s
    FROM provisioning_settings s WHERE s.id = 1
    """

    # the old webhook mode took deliveries from one forge: the named one, else the first GitHub one
    execute """
    UPDATE forges SET mode = 'webhook' WHERE id = (
      SELECT f.id FROM forges f, provisioning_settings s
      WHERE s.id = 1 AND s.mode = 'webhook' AND f.webhook_secret IS NOT NULL
        AND (s.webhook_forge IS NULL OR f.name = s.webhook_forge)
      ORDER BY (f.type <> 'github'), f.inserted_at, f.id LIMIT 1)
    """

    # fixed capacity filled its pools on the first GitHub forge
    execute """
    UPDATE forges SET mode = 'fixed_capacity', runner_scope = s.runner_scope
    FROM provisioning_settings s
    WHERE s.id = 1 AND s.mode = 'fixed_capacity' AND s.runner_scope IS NOT NULL AND forges.id = (
      SELECT id FROM forges WHERE type = 'github' ORDER BY inserted_at, id LIMIT 1)
    """

    # settings never saved: a forge that came with a webhook secret takes its deliveries
    execute """
    UPDATE forges SET mode = 'webhook'
    WHERE webhook_secret IS NOT NULL AND NOT EXISTS (SELECT 1 FROM provisioning_settings)
    """

    alter table(:provisioning_settings) do
      remove :mode
      remove :runner_scope
      remove :runner_group
      remove :poll_interval_s
      remove :webhook_forge
    end
  end

  # the settings come back at their defaults; which forge had which mode is lost
  def down do
    alter table(:provisioning_settings) do
      add :mode, :string, null: false, default: "fixed_capacity"
      add :runner_scope, :string
      add :runner_group, :string
      add :poll_interval_s, :integer, null: false, default: 120
      add :webhook_forge, :string
    end

    alter table(:forges) do
      remove :mode
      remove :runner_scope
      remove :poll_interval_s
    end
  end
end
