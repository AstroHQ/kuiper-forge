defmodule KuiperCoordinator.Repo.Migrations.CreateRunners do
  use Ecto.Migration

  # Runners the coordinator created (Rust: active_runners). The source of truth for pool counts
  # (parity P3), written before CreateRunner is sent (F3).
  def change do
    create table(:runners, primary_key: false) do
      add :runner_name, :string, primary_key: true
      add :agent_id, :string, null: false
      add :vm_name, :string, null: false
      add :runner_scope, :string, null: false
      add :forge, :string, null: false
      add :job_key, :string
      add :job_name, :string
      add :repository, :string
      add :workflow_name, :string
      add :pool, :string
      add :labels, {:array, :string}, null: false, default: []

      timestamps(type: :utc_datetime_usec)
    end

    create index(:runners, [:agent_id])
    create index(:runners, [:job_key])
  end
end
