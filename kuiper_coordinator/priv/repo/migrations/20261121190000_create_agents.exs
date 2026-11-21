defmodule KuiperCoordinator.Repo.Migrations.CreateAgents do
  use Ecto.Migration

  def change do
    create table(:registration_tokens, primary_key: false) do
      add :token, :string, primary_key: true
      add :expires_at, :utc_datetime_usec, null: false
      add :created_by, :string, null: false

      timestamps(type: :utc_datetime_usec, updated_at: false)
    end

    create index(:registration_tokens, [:expires_at])

    create table(:agents, primary_key: false) do
      add :agent_id, :string, primary_key: true
      add :hostname, :string, null: false
      add :agent_type, :string, null: false
      add :labels, {:array, :string}, null: false, default: []
      add :max_vms, :integer, null: false, default: 0
      add :serial_number, :text, null: false
      add :expires_at, :utc_datetime_usec, null: false
      add :revoked_at, :utc_datetime_usec
      add :agent_version, :string
      add :unavailable_at, :utc_datetime_usec

      timestamps(type: :utc_datetime_usec)
    end

    create index(:agents, [:revoked_at])

    # ts is Unix micros from the agent, like the Rust coordinator's agent_logs
    create table(:agent_logs) do
      add :agent_id, :string, null: false
      add :ts, :bigint, null: false
      add :seq, :bigint, null: false, default: 0
      add :level, :smallint, null: false
      add :target, :string, size: 512, null: false, default: ""
      add :message, :text, null: false
      add :inserted_at, :utc_datetime_usec, null: false, default: fragment("now()")
    end

    # an agent re-sends a batch it got no answer for; this key makes the retry a no-op (D25)
    create unique_index(:agent_logs, [:agent_id, :ts, :seq])
    # per-agent cap (newest by id) and age pruning
    create index(:agent_logs, [:agent_id, :id])
    create index(:agent_logs, [:inserted_at])
  end
end
